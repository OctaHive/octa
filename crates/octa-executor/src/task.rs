use std::{
  collections::HashMap,
  hash::{Hash, Hasher},
  path::PathBuf,
  process::Stdio,
  sync::Arc,
  time::Duration,
};

use async_trait::async_trait;
use indexmap::IndexMap;
use sled::Db;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tera::{Context, Tera};
use tokio::{select, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, error, info, Level};

use octa_dag::{Identifiable, DAG};
use octa_octafile::{AllowedRun, SourceStrategies};

use crate::{
  error::{ExecutorError, ExecutorResult},
  executor::ExecutorConfig,
  hash_source::HashSource,
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
    fingerprint: Arc<Db>,
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

/// Represents a single executable task with its configuration and state
#[derive(Debug, Clone)]
pub struct TaskNode {
  // Task identification
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
  pub sources: Option<Vec<String>>,  // Sources for fingerprinting
  pub source_strategy: SourceMethod, // Source validation strategy

  // State management
  pub deps_res: Arc<Mutex<HashMap<String, String>>>, // Dependencies results
  pub dag: Option<DAG<TaskNode>>,                    // Execution plan for complex command
  cmd_type: CmdType,                                 // Type of task for internal use
  cancel_token: CancellationToken,                   // Using for canceling task execution
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
  pub fn new(
    name: String,
    dir: PathBuf,
    vars: Vars,
    cancel_token: CancellationToken,
    cmd: Option<String>,
    tpl: Option<String>,
    sources: Option<Vec<String>>,
    source_strategy: Option<SourceStrategies>,
    silent: Option<bool>,
    ignore_errors: Option<bool>,
    run_mode: Option<AllowedRun>,
    dag: Option<DAG<TaskNode>>,
    cmd_type: Option<CmdType>,
  ) -> Self {
    Self {
      name,
      cmd,
      tpl,
      cancel_token,
      run_mode: run_mode.map(Into::into).unwrap_or(RunMode::Always),
      sources,
      source_strategy: source_strategy.map(Into::into).unwrap_or(SourceMethod::Hash),
      vars,
      dir,
      ignore_errors: ignore_errors.unwrap_or_default(),
      silent: silent.unwrap_or_default(),
      deps_res: Arc::new(Mutex::new(HashMap::default())),
      cmd_type: cmd_type.unwrap_or(CmdType::Normal),
      dag,
    }
  }

  /// Executes a shell command and returns its output
  async fn execute_command(&self, cmd: &str) -> ExecutorResult<String> {
    let rendered_cmd = self.render_template(cmd, None).await?;
    debug!("Execute command: {}", rendered_cmd);

    let dir = self.interpolate_dir(self.dir.clone()).await?;

    // Platform specific command setup
    #[cfg(windows)]
    let command = self.setup_windows_command(&rendered_cmd, &dir);

    #[cfg(not(windows))]
    let command = self.setup_unix_command(&rendered_cmd, &dir);

    self.run_command(command).await
  }

  /// Renders the command template with variables and dependency results
  async fn render_template(
    &self,
    template: &str,
    cache: Option<Arc<Mutex<IndexMap<String, CacheItem>>>>,
  ) -> ExecutorResult<String> {
    // Interpolate vars
    let mut vars = self.vars.clone();
    vars.interpolate().await?;

    if let Some(ref cache) = cache {
      if let Some(cached) = self.check_cache(&vars, &cache).await? {
        return Ok(cached);
      }
    }

    let mut tera = Tera::default();
    let template_name = format!("task_{}", self.name);

    tera.add_raw_template(&template_name, template).map_err(|e| {
      ExecutorError::TemplateParseFailed(format!("Failed to parse template for task {}: {}", self.name, e))
    })?;

    let mut context: Context = vars.clone().into();
    // Add dependency results to template context
    let deps_res = self.deps_res.lock().await;
    context.insert("deps_result", &*deps_res);

    let result = tera.render(&template_name, &context).map_err(|e| {
      ExecutorError::TemplateRenderError(format!("Failed to render template for task {}: {:?}", self.name, e))
    })?;

    if let Some(ref cache) = cache {
      self.update_cache(&result, vars, &cache).await?;
    }

    Ok(result)
  }

  /// Execute command with cancellation support
  async fn run_command(&self, mut command: tokio::process::Command) -> ExecutorResult<String> {
    let mut child = command.spawn()?;

    let output = select! {
        _result = child.wait() => {
          command
              .spawn()?
              .wait_with_output()
              .await?
        },
        _ = self.cancel_token.cancelled() => {
          self.handle_cancellation(&mut child).await?
        }
    };

    self.process_command_output(output).await
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
    let _ = child.kill().await?;

    // Create a new command to get the output
    let _output = child.wait().await?;

    Err(ExecutorError::TaskCancelled(self.name.clone()))
  }

  async fn interpolate_dir(&self, dir: PathBuf) -> ExecutorResult<PathBuf> {
    let dir_str = dir.to_string_lossy();

    if !dir_str.contains("{{") || !dir_str.contains("}}") {
      debug!("Using direct directory path: {}", dir_str);

      Ok(dir)
    } else {
      debug!("Interpolating directory path: {}", dir_str);

      let mut tera = Tera::default();
      let mut vars = self.vars.clone();
      vars.interpolate().await?;

      let context: Context = vars.into();

      let rendered = tera
        .render_str(&dir_str, &context)
        .map_err(|e| ExecutorError::ValueInterpolateError(dir_str.to_string(), e.to_string()))?;

      debug!("Interpolated path: {}", rendered);

      Ok(PathBuf::from(rendered.trim_matches('"'))) // Remove extra quotes from result
    }
  }

  async fn execute_cmd(&self, cmd: String, cache: Arc<Mutex<IndexMap<String, CacheItem>>>) -> ExecutorResult<String> {
    let mut vars = self.vars.clone();
    vars.interpolate().await?;

    if let Some(cached) = self.check_cache(&vars, &cache).await? {
      return Ok(cached);
    }

    match self.execute_command(&cmd).await {
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
  async fn process_command_output(&self, output: std::process::Output) -> ExecutorResult<String> {
    if !output.status.success() && !self.cancel_token.is_cancelled() {
      let error = String::from_utf8_lossy(&output.stderr);
      return Err(ExecutorError::CommandFailed(format!(
        "Task {} failed: {}",
        self.name, error
      )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    self.log_output(&stdout, &stderr);

    Ok(stdout.trim().to_string())
  }

  /// Log command output if not silent
  fn log_output(&self, stdout: &str, stderr: &str) {
    if self.silent {
      return;
    }

    if !stdout.is_empty() {
      info!("{}", stdout.trim());
    }

    if !stderr.is_empty() {
      info!("{}", stderr.trim());
    }
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
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut command = tokio::process::Command::new("cmd");
    command
      .current_dir(dir)
      .args(["/C", cmd])
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    command
  }

  #[cfg(windows)]
  fn terminate_windows_process(&self, child: &mut tokio::process::Child) {
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    if let Some(pid) = child.id() {
      unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle != 0 {
          TerminateProcess(handle, 1);
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
    fingerprint: Arc<Db>,
  ) -> ExecutorResult<String> {
    let executor = Executor::new(dag, ExecutorConfig::default(), Some(cache), fingerprint)?;

    let results = executor.execute(self.cancel_token.clone()).await?;
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
    fingerprint: Arc<Db>,
  ) -> ExecutorResult<String> {
    if !self.check_sources(fingerprint.clone()).await? {
      self.log_info(format!("All files are up to date"));

      return Ok("".to_string());
    };

    self.log_info(format!("Starting task {}", self.name));

    // Debug information about dependency results
    self.debug_log_dependencies().await;

    if let Some(dag) = self.dag.clone() {
      return self.execute_nested_dag(dag, cache, fingerprint).await;
    }

    match (&self.cmd, &self.tpl) {
      // This variant should validate on load octafile stage
      (Some(_), Some(_)) => {
        unreachable!("Both cmd and tpl cannot be Some - should be validated during octafile loading")
      },
      (Some(cmd), None) => self.execute_cmd(cmd.clone(), cache).await,
      (None, Some(tpl)) => {
        let rendered_cmd = self.render_template(tpl, Some(cache)).await?;

        Ok(rendered_cmd)
      },
      // This variant just if we want only run deps
      (None, None) => Ok("".to_string()),
    }
  }
}

impl Identifiable for TaskNode {
  fn id(&self) -> String {
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
  fn create_test_task(name: &str, cmd: Option<&str>, tpl: Option<&str>, run_mode: Option<RunMode>) -> TaskNode {
    TaskNode::new(
      name.to_string(),
      PathBuf::from("."),
      Vars::new(),
      CancellationToken::new(),
      cmd.map(String::from),
      tpl.map(String::from),
      None,
      None,
      Some(false),
      Some(false),
      run_mode.map(|r| match r {
        RunMode::Always => AllowedRun::Always,
        RunMode::Once => AllowedRun::Once,
        RunMode::Changed => AllowedRun::Changed,
      }),
      None,
      None,
    )
  }

  #[tokio::test]
  async fn test_basic_command_execution() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let task = create_test_task("test_task", Some("echo 'hello world'"), None, None);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    let result = task.execute(cache, fingerprint).await.unwrap();
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

    let task = TaskNode::new(
      "template_task".to_string(),
      PathBuf::from("."),
      vars,
      CancellationToken::new(),
      None,
      Some("Hello {{ name }}!".to_owned()),
      None,
      None,
      Some(false),
      Some(false),
      None,
      None,
      None,
    );

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    let result = task.execute(cache, fingerprint).await.unwrap();
    assert_eq!(result, "Hello world!");
  }

  #[tokio::test]
  async fn test_cache_behavior() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");
    let task = create_test_task("cache_task", Some("echo 'cached result'"), None, Some(RunMode::Once));

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    // First execution
    let result1 = task.execute(cache.clone(), fingerprint.clone()).await.unwrap();
    assert_eq!(result1.trim(), "cached result");

    // Second execution should return cached result
    let result2 = task.execute(cache.clone(), fingerprint.clone()).await.unwrap();
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

    let task = TaskNode::new(
      "source_task".to_string(),
      PathBuf::from("."),
      Vars::new(),
      CancellationToken::new(),
      Some("echo 'test'".to_string()),
      None,
      Some(vec![test_file.to_str().unwrap().to_string()]),
      Some(SourceStrategies::Hash),
      Some(false),
      Some(false),
      Some(AllowedRun::Changed),
      None,
      None,
    );

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    // First execution
    let result1 = task.execute(cache.clone(), fingerprint.clone()).await.unwrap();

    // Modify source file
    fs::write(&test_file, "modified content").unwrap();

    // Second execution should run again due to source changes
    let result2 = task.execute(cache.clone(), fingerprint.clone()).await.unwrap();
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

    let result = task.execute(cache, fingerprint).await;
    assert!(matches!(result, Err(ExecutorError::TaskFailed(_))));
  }

  #[tokio::test]
  async fn test_task_cancellation() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let cancel_token = CancellationToken::new();
    let task = TaskNode::new(
      "long_task".to_string(),
      PathBuf::from("."),
      Vars::new(),
      cancel_token.clone(),
      Some("sleep 5".to_string()),
      None,
      None,
      None,
      Some(false),
      Some(false),
      None,
      None,
      None,
    );

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    // Cancel the task after a short delay
    let cancel_handle = tokio::spawn({
      let cancel_token = cancel_token.clone();
      async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_token.cancel();
      }
    });

    let result = task.execute(cache, fingerprint).await;

    println!("{:?}", result);
    assert!(matches!(result, Err(ExecutorError::TaskCancelled(_))));

    cancel_handle.await.unwrap();
  }

  #[tokio::test]
  async fn test_ignore_errors() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let task = TaskNode::new(
      "ignore_error_task".to_string(),
      PathBuf::from("."),
      Vars::new(),
      CancellationToken::new(),
      Some("nonexistent_command".to_string()),
      None,
      None,
      None,
      Some(false),
      Some(true), // ignore_errors = true
      None,
      None,
      None,
    );

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    let result = task.execute(cache, fingerprint).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "");
  }

  #[tokio::test]
  async fn test_dependency_results() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let task = create_test_task("dep_task", None, Some("Result: {{ deps_result.dep1 }}"), None);

    task.set_result("dep1".to_string(), "dep_output".to_string()).await;

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    let result = task.execute(cache, fingerprint).await.unwrap();
    assert_eq!(result, "Result: dep_output");
  }

  #[tokio::test]
  async fn test_nested_dag_execution() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let mut nested_dag = DAG::new();
    let nested_task = create_test_task("nested_task", Some("echo 'nested'"), None, None);
    nested_dag.add_node(Arc::new(nested_task));

    let parent_task = TaskNode::new(
      "parent_task".to_string(),
      PathBuf::from("."),
      Vars::new(),
      CancellationToken::new(),
      None,
      None,
      None,
      None,
      Some(false),
      Some(false),
      None,
      Some(nested_dag),
      None,
    );

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);

    let result = parent_task.execute(cache, fingerprint).await.unwrap();
    assert_eq!(result.trim(), "nested");
  }
}
