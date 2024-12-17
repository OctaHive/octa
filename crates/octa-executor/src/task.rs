use std::{
  borrow::Cow,
  collections::HashMap,
  env,
  hash::{Hash, Hasher},
  path::PathBuf,
  process::Stdio,
  sync::Arc,
  time::Duration,
};

use async_trait::async_trait;
use indexmap::IndexMap;
use sled::Db;
use tera::{Context, Tera};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::{select, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, error, info, Level};

use octa_dag::{Identifiable, DAG};
use octa_octafile::{AllowedRun, SourceStrategies};

use crate::{
  envs::Envs,
  error::{ExecutorError, ExecutorResult},
  executor::ExecutorConfig,
  hash_source::HashSource,
  summary::Summary,
  timestamp_source::TimestampSource,
  vars::Vars,
  Executor,
};

/// Core traits and types
#[async_trait]
pub trait Executable<T> {
  async fn execute(
    &self,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    summary: Arc<Summary>,
    fingerprint: Arc<Db>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String>;
  async fn set_result(&self, task_name: String, res: String);
}

#[async_trait]
pub trait SourceStrategy: Send {
  async fn is_changed(&self, sources: Vec<String>) -> ExecutorResult<bool>;
}

pub trait TaskItem {
  fn run_mode(&self) -> RunMode;
}

/// Enums for task configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMethod {
  Timestamp,
  Hash,
}

impl From<SourceStrategies> for SourceMethod {
  fn from(value: SourceStrategies) -> Self {
    match value {
      SourceStrategies::Timestamp => SourceMethod::Timestamp,
      SourceStrategies::Hash => SourceMethod::Hash,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
  Always,
  Once,
  Changed,
}

impl From<AllowedRun> for RunMode {
  fn from(value: AllowedRun) -> Self {
    match value {
      AllowedRun::Once => RunMode::Once,
      AllowedRun::Always => RunMode::Always,
      AllowedRun::Changed => RunMode::Changed,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdType {
  Normal,
  Internal,
}

/// Cache implementation
#[derive(Debug)]
pub struct CacheItem {
  result: String,
  vars: Vars,
}

impl CacheItem {
  pub fn new(result: String, vars: Vars) -> Self {
    Self { result, vars }
  }
}

pub struct TaskConfig {
  // Task identification
  pub id: String,
  pub name: String,

  // Execution configuration
  pub cmd: Option<String>, // Command to execute
  pub tpl: Option<String>, // Template to render
  pub dir: PathBuf,        // Working directory
  pub ignore_errors: bool, // Whether to continue on error
  pub silent: bool,        // Should task print to stdout or stderr

  // Runtime behavior
  pub run_mode: RunMode,             // Run mode
  pub vars: Vars,                    // Task variables
  pub envs: Envs,                    // Task environments
  pub sources: Option<Vec<String>>,  // Sources for fingerprinting
  pub source_strategy: SourceMethod, // Source validation strategy

  // State management
  pub dag: Option<DAG<TaskNode>>, // Execution plan for complex command
  cmd_type: CmdType,              // Type of task for internal use
}

impl TaskConfig {
  pub fn builder() -> TaskConfigBuilder {
    TaskConfigBuilder::default()
  }
}

#[derive(Default)]
pub struct TaskConfigBuilder {
  id: Option<String>,
  name: Option<String>,

  pub cmd: Option<String>,
  pub tpl: Option<String>,
  pub dir: Option<PathBuf>,
  pub ignore_errors: Option<bool>,
  pub silent: Option<bool>,

  pub run_mode: Option<RunMode>,
  pub vars: Option<Vars>,
  pub envs: Option<Envs>,
  pub sources: Option<Vec<String>>,
  pub source_strategy: Option<SourceMethod>,

  pub cmd_type: Option<CmdType>,
  pub dag: Option<DAG<TaskNode>>,
}

impl TaskConfigBuilder {
  pub fn name(mut self, name: impl Into<String>) -> Self {
    self.name = Some(name.into());
    self
  }

  pub fn id(mut self, id: impl Into<String>) -> Self {
    self.id = Some(id.into());
    self
  }

  pub fn cmd(mut self, cmd: Option<impl Into<String>>) -> Self {
    self.cmd = cmd.map(Into::into);
    self
  }

  pub fn tpl(mut self, tpl: Option<String>) -> Self {
    self.tpl = tpl;
    self
  }

  pub fn sources(mut self, sources: Option<Vec<String>>) -> Self {
    self.sources = sources;
    self
  }

  pub fn vars(mut self, vars: Vars) -> Self {
    self.vars = Some(vars);
    self
  }

  pub fn envs(mut self, envs: Envs) -> Self {
    self.envs = Some(envs);
    self
  }

  pub fn dir(mut self, dir: impl Into<PathBuf>) -> Self {
    self.dir = Some(dir.into());
    self
  }

  pub fn ignore_errors(mut self, ignore_errors: Option<bool>) -> Self {
    self.ignore_errors = ignore_errors;
    self
  }

  pub fn silent(mut self, silent: Option<bool>) -> Self {
    self.silent = silent;
    self
  }

  pub fn run_mode(mut self, run_mode: Option<impl Into<RunMode>>) -> Self {
    self.run_mode = run_mode.map(Into::into);
    self
  }

  pub fn source_strategy(mut self, source_strategy: Option<impl Into<SourceMethod>>) -> Self {
    self.source_strategy = source_strategy.map(Into::into);
    self
  }

  pub fn cmd_type(mut self, cmd_type: Option<CmdType>) -> Self {
    self.cmd_type = cmd_type;
    self
  }

  pub fn dag(mut self, dag: Option<DAG<TaskNode>>) -> Self {
    self.dag = dag;
    self
  }

  pub fn build(self) -> Result<TaskConfig, &'static str> {
    Ok(TaskConfig {
      id: self.id.ok_or("Missing mandatory field: id")?,
      name: self.name.ok_or("Missing mandatory field: name")?,
      cmd: self.cmd,
      tpl: self.tpl,
      dir: self.dir.ok_or("Missing mandatory field: dir")?,
      ignore_errors: self.ignore_errors.unwrap_or(false),
      silent: self.silent.unwrap_or(false),
      run_mode: self.run_mode.unwrap_or(RunMode::Always),
      vars: self.vars.ok_or("Missing mandatory field: vars")?,
      envs: self.envs.ok_or("Missing mandatory field: envs")?,
      sources: self.sources,
      source_strategy: self.source_strategy.unwrap_or(SourceMethod::Hash),
      cmd_type: self.cmd_type.unwrap_or(CmdType::Normal),
      dag: self.dag,
    })
  }
}

/// Represents a single executable task with its configuration and state
#[derive(Debug, Clone)]
pub struct TaskNode {
  // Task identification
  pub id: String,
  pub name: String, // Task name

  // Execution configuration
  pub cmd: Option<String>, // Command to execute
  pub tpl: Option<String>, // Template to render
  pub dir: PathBuf,        // Working directory
  pub ignore_errors: bool, // Whether to continue on error
  pub silent: bool,        // Should task print to stdout or stderr

  // Runtime behavior
  pub run_mode: RunMode,             // Run mode
  pub vars: Vars,                    // Task variables
  pub envs: Envs,                    // Task environments
  pub sources: Option<Vec<String>>,  // Sources for fingerprinting
  pub source_strategy: SourceMethod, // Source validation strategy

  // State management
  pub deps_res: Arc<Mutex<HashMap<String, String>>>, // Dependencies results
  pub dag: Option<DAG<TaskNode>>,                    // Execution plan for complex command
  cmd_type: CmdType,                                 // Type of task for internal use
}

// Implement equality based on task ID
impl Eq for TaskNode {}

impl PartialEq for TaskNode {
  fn eq(&self, other: &Self) -> bool {
    self.name == other.name
  }
}

// Implement hashing based on task name
impl Hash for TaskNode {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.name.hash(state);
  }
}

impl TaskNode {
  pub fn new(config: TaskConfig) -> Self {
    Self {
      id: config.id,
      name: config.name,
      cmd: config.cmd,
      tpl: config.tpl,
      run_mode: config.run_mode,
      sources: config.sources,
      source_strategy: config.source_strategy,
      vars: config.vars,
      envs: config.envs,
      dir: config.dir,
      ignore_errors: config.ignore_errors,
      silent: config.silent,
      deps_res: Arc::new(Mutex::new(HashMap::default())),
      cmd_type: config.cmd_type,
      dag: config.dag,
    }
  }

  /// Executes a shell command and returns its output
  async fn execute_command(&self, cmd: &str, dry: bool, cancel_token: CancellationToken) -> ExecutorResult<String> {
    let rendered_cmd = self.render_template(cmd, None, dry).await?;
    debug!("Execute command: {}", rendered_cmd);

    if dry {
      info!("Execute command in dry mode: {}", rendered_cmd);

      return Ok("".to_owned());
    }

    let dir = self.interpolate_dir(self.dir.clone(), dry).await?;

    // Platform specific command setup
    #[cfg(windows)]
    let command = self.setup_windows_command(&rendered_cmd, &dir);

    #[cfg(not(windows))]
    let command = self.setup_unix_command(&rendered_cmd, &dir);

    self.run_command(command, cancel_token).await
  }

  /// Renders the command template with variables and dependency results
  async fn render_template(
    &self,
    template: &str,
    cache: Option<Arc<Mutex<IndexMap<String, CacheItem>>>>,
    dry: bool,
  ) -> ExecutorResult<String> {
    // Interpolate vars
    let mut vars = self.vars.clone();
    vars.expand(dry).await?;

    let mut envs = self.envs.clone();
    envs.expand().await?;

    if let Some(ref cache) = cache {
      if let Some(cached) = self.check_cache(&vars, cache).await? {
        return Ok(cached);
      }
    }

    let get_env = |name: &str| match envs.get(name) {
      Some(val) => Some(Cow::Borrowed(val.as_str())),
      None => match env::var(name) {
        Ok(val) => Some(Cow::Owned(val)),
        Err(_) => None,
      },
    };

    let val = shellexpand::env_with_context_no_errors(&template, get_env);

    let mut tera = Tera::default();
    let template_name = format!("task_{}", self.name);

    tera.add_raw_template(&template_name, val.as_ref()).map_err(|e| {
      ExecutorError::TemplateParseFailed(format!("Failed to parse template for task {}: {:?}", self.name, e))
    })?;

    let mut context: Context = vars.clone().into();
    // Add dependency results to template context
    let deps_res = self.deps_res.lock().await;
    context.insert("deps_result", &*deps_res);

    let result = tera.render(&template_name, &context).map_err(|e| {
      ExecutorError::TemplateRenderError(format!("Failed to render template for task {}: {:?}", self.name, e))
    })?;

    if let Some(ref cache) = cache {
      self.update_cache(&result, vars, cache).await?;
    }

    Ok(result)
  }

  /// Execute command with cancellation support
  async fn run_command(
    &self,
    mut command: tokio::process::Command,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    let mut child = command.spawn()?;
    let mut output = Vec::new();
    let mut errors = Vec::new();

    // Handle stdout
    let stdout_handle = if let Some(stdout) = child.stdout.take() {
      let reader = BufReader::new(stdout);
      let mut lines = reader.lines();
      let is_silent = self.silent;
      Some(tokio::spawn(async move {
        let mut captured = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap_or(None) {
          if !line.is_empty() {
            captured.push(line.clone());
            if !is_silent {
              println!("{}", line);
            }
          }
        }
        captured
      }))
    } else {
      None
    };

    // Handle stderr
    let stderr_handle = if let Some(stderr) = child.stderr.take() {
      let reader = BufReader::new(stderr);
      let mut lines = reader.lines();
      let is_silent = self.silent;
      Some(tokio::spawn(async move {
        let mut captured = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap_or(None) {
          if !line.is_empty() {
            captured.push(line.clone());
            if !is_silent {
              eprintln!("{}", line);
            }
          }
        }
        captured
      }))
    } else {
      None
    };

    let result = select! {
      _ = cancel_token.cancelled() => {
        self.handle_cancellation(&mut child).await?
      }
      result = async {
        // Wait for process completion and stream processing
        let status = child.wait().await?;

        // Collect stdout if available
        if let Some(handle) = stdout_handle {
          if let Ok(lines) = handle.await {
            output.extend(lines);
          }
        }

        // Collect stderr if available
        if let Some(handle) = stderr_handle {
          if let Ok(lines) = handle.await {
            errors.extend(lines);
          }
        }

        // Create synthetic Output struct
        Ok::<std::process::Output, std::io::Error>(std::process::Output {
          status,
          stdout: output.join("\n").into_bytes(),
          stderr: errors.join("\n").into_bytes(),
        })
      } => result?
    };

    self.process_command_output(result, cancel_token).await
  }

  /// Handle task cancellation
  async fn handle_cancellation(&self, child: &mut tokio::process::Child) -> ExecutorResult<std::process::Output> {
    self.log_info(format!("Shutting down task {}", self.name));

    // Platform-specific process termination
    #[cfg(windows)]
    self.terminate_windows_process(child);

    #[cfg(unix)]
    self.terminate_unix_process(child);

    // Give the process time to clean up
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Force kill if still running
    child.kill().await?;

    // Create a new command to get the output
    let _output = child.wait().await?;

    Err(ExecutorError::TaskCancelled(self.name.clone()))
  }

  async fn interpolate_dir(&self, dir: PathBuf, dry: bool) -> ExecutorResult<PathBuf> {
    let dir_str = dir.to_string_lossy();

    if !dir_str.contains("{{") || !dir_str.contains("}}") {
      debug!("Using direct directory path: {}", dir_str);

      Ok(dir)
    } else {
      debug!("Expanding directory path: {}", dir_str);

      let mut tera = Tera::default();
      let mut vars = self.vars.clone();
      vars.expand(dry).await?;

      let context: Context = vars.into();

      let rendered = tera
        .render_str(&dir_str, &context)
        .map_err(|e| ExecutorError::ValueExpandError(dir_str.to_string(), e.to_string()))?;

      debug!("Expanded path: {}", rendered);

      Ok(PathBuf::from(rendered.trim_matches('"'))) // Remove extra quotes from result
    }
  }

  async fn execute_cmd(
    &self,
    cmd: String,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    let mut vars = self.vars.clone();
    vars.expand(dry).await?;

    if let Some(cached) = self.check_cache(&vars, &cache).await? {
      return Ok(cached);
    }

    match self.execute_command(&cmd, dry, cancel_token).await {
      Ok(result) => {
        self.update_cache(&result, vars, &cache).await?;
        debug!("Completed task {}", self.name);

        Ok(result)
      },
      Err(ExecutorError::TaskCancelled(e)) => Err(ExecutorError::TaskCancelled(e)),
      Err(e) => self.handle_execution_error(e),
    }
  }

  fn log_info(&self, message: String) {
    if self.cmd_type != CmdType::Internal {
      info!("{}", message);
    }
  }

  async fn check_sources(&self, fingerprint: Arc<Db>) -> ExecutorResult<bool> {
    if let Some(sources) = &self.sources {
      let strategy: Box<dyn SourceStrategy + Send> = match self.source_strategy {
        SourceMethod::Hash => Box::new(HashSource::new(fingerprint.clone())),
        SourceMethod::Timestamp => Box::new(TimestampSource::new(fingerprint.clone())),
      };
      strategy.is_changed(sources.clone()).await
    } else {
      Ok(true)
    }
  }

  /// Process command output and handle errors
  async fn process_command_output(
    &self,
    output: std::process::Output,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    if !output.status.success() && !cancel_token.is_cancelled() {
      let error = String::from_utf8_lossy(&output.stderr);
      return Err(ExecutorError::CommandFailed(format!(
        "Task {} failed: {}",
        self.name, error
      )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    Ok(stdout.trim().to_string())
  }

  /// Check if result is cached
  async fn check_cache(
    &self,
    vars: &Vars,
    cache: &Arc<Mutex<IndexMap<String, CacheItem>>>,
  ) -> ExecutorResult<Option<String>> {
    if self.run_mode == RunMode::Always {
      return Ok(None);
    }

    let cache_lock = cache.lock().await;
    if let Some(cached_result) = cache_lock.get(&self.name) {
      if &cached_result.vars == vars {
        debug!("Cache hit for task: {}", self.name);
        return Ok(Some(cached_result.result.clone()));
      }
    }
    Ok(None)
  }

  /// Update cache with new result
  async fn update_cache(
    &self,
    result: &str,
    vars: Vars,
    cache: &Arc<Mutex<IndexMap<String, CacheItem>>>,
  ) -> ExecutorResult<()> {
    if self.run_mode != RunMode::Always {
      let mut cache_lock = cache.lock().await;
      cache_lock.insert(self.name.clone(), CacheItem::new(result.to_string(), vars.clone()));
      debug!("Cached result for task: {}", self.name);
    }
    Ok(())
  }

  /// Platform-specific command setup for Windows
  #[cfg(windows)]
  fn setup_windows_command(&self, cmd: &str, dir: &PathBuf) -> tokio::process::Command {
    #[allow(unused_imports)]
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut command = tokio::process::Command::new("cmd");
    command
      .current_dir(dir)
      .args(["/C", cmd])
      .envs(self.envs.clone())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    command
  }

  #[cfg(windows)]
  fn terminate_windows_process(&self, child: &mut tokio::process::Child) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    if let Some(pid) = child.id() {
      unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle as HANDLE != INVALID_HANDLE_VALUE {
          let handle_ptr = handle as HANDLE;
          TerminateProcess(handle_ptr, 1);
          CloseHandle(handle_ptr);
        }
      }
    }
  }

  /// Platform-specific command setup for Unix
  #[cfg(not(windows))]
  fn setup_unix_command(&self, cmd: &str, dir: &PathBuf) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
      .current_dir(dir)
      .arg("-c")
      .arg(cmd)
      .envs(self.envs.clone())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .process_group(0);
    command
  }

  #[cfg(unix)]
  fn terminate_unix_process(&self, child: &mut tokio::process::Child) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    if let Some(pid) = child.id() {
      self.log_info(format!("Kill task with pid {}", pid));
      let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
    }
  }

  async fn debug_log_dependencies(&self) {
    if enabled!(Level::DEBUG) {
      let deps = self.deps_res.lock().await;
      for (name, res) in &*deps {
        debug!("Dependency {} results: {}", name, res);
      }
    }
  }

  async fn execute_nested_dag(
    &self,
    dag: DAG<TaskNode>,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    summary: Arc<Summary>,
    fingerprint: Arc<Db>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    let executor = Executor::new(
      dag,
      ExecutorConfig::default(),
      Some(cache),
      fingerprint,
      dry,
      Some(summary),
    )?;

    let results = executor.execute(cancel_token.clone(), &self.name).await?;
    Ok(results.join("\n"))
  }

  /// Handle execution errors
  fn handle_execution_error(&self, error: ExecutorError) -> ExecutorResult<String> {
    if self.ignore_errors {
      error!("Task {} failed but errors ignored. Error: {}", self.name, error);
      Ok("".to_string())
    } else {
      Err(ExecutorError::TaskFailed(error.to_string()))
    }
  }
}

#[async_trait]
impl Executable<TaskNode> for TaskNode {
  /// Stores the result of a dependent task
  async fn set_result(&self, task_name: String, res: String) {
    let mut deps_res = self.deps_res.lock().await;

    deps_res.insert(task_name, res);
  }

  /// Executes the task and returns the result
  async fn execute(
    &self,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    summary: Arc<Summary>,
    fingerprint: Arc<Db>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    if !self.check_sources(fingerprint.clone()).await? {
      self.log_info(format!("Task {} are up to date", self.name));

      return Ok("".to_string());
    };

    self.log_info(format!("Starting task {}", self.name));

    // Debug information about dependency results
    self.debug_log_dependencies().await;

    if let Some(dag) = self.dag.clone() {
      return self
        .execute_nested_dag(dag, cache, summary, fingerprint, dry, cancel_token.clone())
        .await;
    }

    match (&self.cmd, &self.tpl) {
      // This variant should validate on load octafile stage
      (Some(_), Some(_)) => {
        unreachable!("Both cmd and tpl cannot be Some - should be validated during octafile loading")
      },
      (Some(cmd), None) => self.execute_cmd(cmd.clone(), cache, dry, cancel_token.clone()).await,
      (None, Some(tpl)) => {
        let rendered_cmd = self.render_template(tpl, Some(cache), dry).await?;

        Ok(rendered_cmd)
      },
      // This variant just if we want only run deps
      (None, None) => Ok("".to_string()),
    }
  }
}

impl Identifiable for TaskNode {
  fn id(&self) -> String {
    self.id.clone()
  }

  fn name(&self) -> String {
    self.name.clone()
  }
}

impl TaskItem for TaskNode {
  fn run_mode(&self) -> RunMode {
    self.run_mode.clone()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;

  // Helper function to create a test TaskNode
  fn create_test_task(name: &str, cmd: Option<&str>, tpl: Option<String>, run_mode: Option<RunMode>) -> TaskNode {
    let task_config = TaskConfig::builder()
      .id(name.to_string())
      .name(name.to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .tpl(tpl)
      .cmd(cmd)
      .run_mode(Some(run_mode.unwrap_or(RunMode::Always)))
      .build()
      .unwrap();

    TaskNode::new(task_config)
  }

  #[tokio::test]
  async fn test_basic_command_execution() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let summary = Arc::new(Summary::new());

    let task = create_test_task("test_task", Some("echo hello world"), None, None);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    let result = task
      .execute(cache, summary, fingerprint, false, CancellationToken::new())
      .await
      .unwrap();
    assert_eq!(result.trim(), "hello world");
  }

  #[tokio::test]
  async fn test_template_rendering() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let mut vars = Vars::new();
    vars.insert(&"name", &"world");

    let task_config = TaskConfig::builder()
      .id("template_task".to_string())
      .name("template_task".to_string())
      .dir(PathBuf::from("."))
      .vars(vars)
      .envs(Envs::new())
      .tpl(Some("Hello {{ name }}!".to_owned()))
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    let result = task
      .execute(cache, summary, fingerprint, false, CancellationToken::new())
      .await
      .unwrap();
    assert_eq!(result, "Hello world!");
  }

  #[tokio::test]
  async fn test_cache_behavior() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");
    let task = create_test_task("cache_task", Some("echo cached result"), None, Some(RunMode::Once));

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    // First execution
    let result1 = task
      .execute(
        cache.clone(),
        summary.clone(),
        fingerprint.clone(),
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result1.trim(), "cached result");

    // Second execution should return cached result
    let result2 = task
      .execute(
        cache.clone(),
        summary,
        fingerprint.clone(),
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result1, result2);
  }

  #[tokio::test]
  async fn test_source_changes() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let temp_dir = TempDir::new().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    fs::write(&test_file, "initial content").unwrap();

    let task_config = TaskConfig::builder()
      .id("source_task".to_string())
      .name("source_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .cmd(Some("echo 'test'".to_string()))
      .run_mode(Some(AllowedRun::Changed))
      .sources(Some(vec![test_file.to_str().unwrap().to_string()]))
      .source_strategy(Some(SourceStrategies::Hash))
      .tpl(None)
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    // First execution
    let result1 = task
      .execute(
        cache.clone(),
        summary.clone(),
        fingerprint.clone(),
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();

    // Modify source file
    fs::write(&test_file, "modified content").unwrap();

    // Second execution should run again due to source changes
    let result2 = task
      .execute(
        cache.clone(),
        summary,
        fingerprint.clone(),
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result1, result2);
  }

  #[tokio::test]
  async fn test_error_handling() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let task = create_test_task("error_task", Some("nonexistent_command"), None, None);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    let result = task
      .execute(cache, summary, fingerprint, false, CancellationToken::new())
      .await;
    assert!(matches!(result, Err(ExecutorError::TaskFailed(_))));
  }

  #[tokio::test]
  async fn test_task_cancellation() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let cancel_token = CancellationToken::new();
    let task_config = TaskConfig::builder()
      .id("long_task".to_string())
      .name("long_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .cmd(Some("sleep 5".to_string()))
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    // Cancel the task after a short delay
    let cancel_handle = tokio::spawn({
      let cancel_token = cancel_token.clone();
      async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_token.cancel();
      }
    });

    let result = task.execute(cache, summary, fingerprint, false, cancel_token).await;
    assert!(matches!(result, Err(ExecutorError::TaskCancelled(_))));

    cancel_handle.await.unwrap();
  }

  #[tokio::test]
  async fn test_ignore_errors() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let task_config = TaskConfig::builder()
      .id("ignore_error_task".to_string())
      .name("ignore_error_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .cmd(Some("nonexistent_command".to_string()))
      .ignore_errors(Some(true))
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    let result = task
      .execute(cache, summary, fingerprint, false, CancellationToken::new())
      .await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "");
  }

  #[tokio::test]
  async fn test_dependency_results() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let task = create_test_task(
      "dep_task",
      None,
      Some("Result: {{ deps_result.dep1 }}".to_owned()),
      None,
    );

    task.set_result("dep1".to_string(), "dep_output".to_string()).await;

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    let result = task
      .execute(cache, summary, fingerprint, false, CancellationToken::new())
      .await
      .unwrap();
    assert_eq!(result, "Result: dep_output");
  }

  #[tokio::test]
  async fn test_nested_dag_execution() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let mut nested_dag = DAG::new();
    let nested_task = create_test_task("nested_task", Some("echo nested"), None, None);
    nested_dag.add_node(Arc::new(nested_task));

    let task_config = TaskConfig::builder()
      .id("parent_task".to_string())
      .name("parent_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .dag(Some(nested_dag))
      .build()
      .unwrap();

    let parent_task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let summary = Arc::new(Summary::new());

    let result = parent_task
      .execute(cache, summary, fingerprint, false, CancellationToken::new())
      .await
      .unwrap();
    assert_eq!(result.trim(), "nested");
  }
}
