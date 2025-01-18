use std::{
  collections::HashMap,
  env,
  hash::{Hash, Hasher},
  io,
  path::PathBuf,
  sync::Arc,
};

use async_trait::async_trait;
use dunce::canonicalize;
use indexmap::IndexMap;
use octa_plugin::protocol::ServerResponse;
use octa_plugin_manager::plugin_manager::PluginManager;
use serde_json::Value;
use sled::Db;
use tera::{Context, Tera};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, error, info, Level};

use octa_dag::Identifiable;
use octa_octafile::{AllowedRun, SourceStrategies};

use crate::{
  envs::Envs,
  error::{ExecutorError, ExecutorResult},
  hash_source::HashSource,
  timestamp_source::TimestampSource,
  vars::Vars,
};

/// Core traits and types
#[async_trait]
pub trait Executable<T> {
  async fn execute(
    &self,
    plugin_manager: Arc<PluginManager>,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    fingerprint: Arc<Db>,
    dry: bool,
    force: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String>;
  async fn set_result(&self, task_name: String, res: String);
  async fn bypass_result(&self, result: HashMap<String, String>);
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
  pub dep_name: String,

  // Execution configuration
  pub dir: PathBuf,        // Working directory
  pub ignore_errors: bool, // Whether to continue on error
  pub silent: bool,        // Should task print to stdout or stderr

  // Runtime behavior
  pub run_mode: RunMode,                  // Run mode
  pub vars: Vars,                         // Task variables
  pub envs: Envs,                         // Task environments
  pub sources: Option<Vec<String>>,       // Sources for fingerprinting
  pub source_strategy: SourceMethod,      // Source validation strategy
  pub preconditions: Option<Vec<String>>, // Task preconditions

  // State management
  cmd_type: CmdType, // Type of task for internal use

  extra: HashMap<String, Value>,
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
  dep_name: Option<String>,

  pub dir: Option<PathBuf>,
  pub ignore_errors: Option<bool>,
  pub silent: Option<bool>,

  pub run_mode: Option<RunMode>,
  pub vars: Option<Vars>,
  pub envs: Option<Envs>,
  pub sources: Option<Vec<String>>,
  pub source_strategy: Option<SourceMethod>,
  pub preconditions: Option<Vec<String>>,

  pub cmd_type: Option<CmdType>,

  pub extra: HashMap<String, Value>,
}

impl TaskConfigBuilder {
  pub fn name(mut self, name: impl Into<String>) -> Self {
    self.name = Some(name.into());
    self
  }

  pub fn dep_name(mut self, dep_name: impl Into<String>) -> Self {
    self.dep_name = Some(dep_name.into());
    self
  }

  pub fn id(mut self, id: impl Into<String>) -> Self {
    self.id = Some(id.into());
    self
  }

  pub fn sources(mut self, sources: Option<Vec<String>>) -> Self {
    self.sources = sources;
    self
  }

  pub fn preconditions(mut self, preconditions: Option<Vec<String>>) -> Self {
    self.preconditions = preconditions;
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

  pub fn extra(mut self, extra: HashMap<String, Value>) -> Self {
    self.extra = extra;
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

  pub fn build(self) -> Result<TaskConfig, &'static str> {
    let dir = match self.cmd_type {
      Some(CmdType::Normal) => self.dir,
      Some(CmdType::Internal) => Some(env::current_dir().unwrap()),
      None => self.dir,
    };

    Ok(TaskConfig {
      id: self.id.ok_or("Missing mandatory field: id")?,
      name: self.name.ok_or("Missing mandatory field: name")?,
      dep_name: self.dep_name.ok_or("Missing mandatory field: dep_name")?,
      dir: dir.ok_or("Missing mandatory field: dir")?,
      ignore_errors: self.ignore_errors.unwrap_or(false),
      silent: self.silent.unwrap_or(false),
      run_mode: self.run_mode.unwrap_or(RunMode::Always),
      vars: self.vars.unwrap_or_default(),
      envs: self.envs.unwrap_or_default(),
      sources: self.sources,
      preconditions: self.preconditions,
      source_strategy: self.source_strategy.unwrap_or(SourceMethod::Hash),
      cmd_type: self.cmd_type.unwrap_or(CmdType::Normal),
      extra: self.extra,
    })
  }
}

/// Represents a single executable task with its configuration and state
#[derive(Debug, Clone)]
pub struct TaskNode {
  // Task identification
  pub id: String,       // Task uniq id
  pub name: String,     // Task name
  pub dep_name: String, // Name of task in deps

  // Execution configuration
  pub dir: PathBuf,        // Working directory
  pub ignore_errors: bool, // Whether to continue on error
  pub silent: bool,        // Should task print to stdout or stderr

  // Runtime behavior
  pub run_mode: RunMode,                  // Run mode
  pub vars: Vars,                         // Task variables
  pub envs: Envs,                         // Task environments
  pub sources: Option<Vec<String>>,       // Sources for fingerprinting
  pub source_strategy: SourceMethod,      // Source validation strategy
  pub preconditions: Option<Vec<String>>, // Task run preconditions

  // State management
  pub deps_res: Arc<Mutex<HashMap<String, String>>>, // Dependencies results
  cmd_type: CmdType,                                 // Type of task for internal use

  extra: HashMap<String, Value>,
}

// Implement equality based on task ID
impl Eq for TaskNode {}

impl PartialEq for TaskNode {
  fn eq(&self, other: &Self) -> bool {
    self.id == other.id
  }
}

// Implement hashing based on task name
impl Hash for TaskNode {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.id.hash(state);
  }
}

impl TaskNode {
  pub fn new(config: TaskConfig) -> Self {
    Self {
      id: config.id,
      name: config.name,
      dep_name: config.dep_name,
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
      preconditions: config.preconditions,
      extra: config.extra,
    }
  }

  #[allow(clippy::too_many_arguments)]
  async fn execute_plugin_command(
    &self,
    plugin_manager: Arc<PluginManager>,
    plugin_name: &str,
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    silent: bool,
    cancel_token: CancellationToken,
  ) -> io::Result<(i32, String, String)> {
    // Changed return type
    let mut output = String::new();
    let mut errors = String::new();
    let mut exit_code = None;

    // Connect to plugin
    #[cfg(not(windows))]
    let plugin_name = &format!("octa_plugin_{}", plugin_name);
    #[cfg(windows)]
    let plugin_name = &format!("octa_plugin_{}.exe", plugin_name);

    let client = plugin_manager.get_client(plugin_name).await.unwrap();
    let mut client_guard = client.lock().await;
    let client = client_guard.as_mut().unwrap();

    // Use a cleanup flag to track if we need to shut down
    let mut needs_cleanup = false;
    let result = async {
      // Start command execution with cancellation support
      let command_id = client
        .execute(command.clone(), args, dir, vars, envs, cancel_token.clone())
        .await
        .map_err(io::Error::from)?;

      // Process output until command completes
      loop {
        match client.receive_output(&cancel_token).await {
          Ok(Some(response)) => match response {
            ServerResponse::Stdout { id, line } if id == command_id => {
              if !silent {
                println!("{}", line.trim());
              }
              output.push_str(line.trim());
              output.push('\n');
            },
            ServerResponse::Stderr { id, line } if id == command_id => {
              if !silent {
                eprintln!("{}", line.trim());
              }
              errors.push_str(line.trim());
              errors.push('\n');
            },
            ServerResponse::ExitStatus { id, code } if id == command_id => {
              exit_code = Some(code);
              break;
            },
            ServerResponse::Error { id, message } if id == command_id => {
              return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Plugin error: {}", message),
              ));
            },
            _ => {},
          },
          Ok(None) => {
            return Err(io::Error::new(
              io::ErrorKind::ConnectionAborted,
              "Plugin connection closed unexpectedly",
            ));
          },
          Err(e) => {
            if cancel_token.is_cancelled() {
              return Err(io::Error::new(io::ErrorKind::Interrupted, "Command cancelled"));
            }

            needs_cleanup = true;
            return Err(io::Error::from(e));
          },
        }
      }

      Ok((exit_code.unwrap_or(-1), output, errors))
    }
    .await;

    // Perform cleanup if needed
    if needs_cleanup {
      let _ = client.shutdown().await;
    }

    result
  }

  /// Executes a shell command and returns its output
  async fn execute_command(
    &self,
    plugin_manager: Arc<PluginManager>,
    cmd: &str,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    let rendered_cmd = self
      .render_template(plugin_manager.clone(), cmd, None, dry, cancel_token.clone())
      .await?;

    debug!("Execute command: {}", rendered_cmd);

    if dry {
      info!("Execute command in dry mode: {}", rendered_cmd);
      return Ok("".to_owned());
    }

    let dir = self.interpolate_dir(self.dir.clone(), dry).await?;
    let dir = canonicalize(dir)?;

    // Interpolate vars
    let mut vars = self.vars.clone();
    vars.expand(dry).await?;

    debug!("Using shell plugin for command execution");
    match self
      .execute_plugin_command(
        plugin_manager,
        "shell",
        rendered_cmd,
        vec![],
        dir,
        vars.to_hashmap(),
        self.envs.clone().into(),
        self.silent,
        cancel_token.clone(),
      )
      .await
    {
      Ok((code, stdout, stderr)) => {
        if code != 0 && !cancel_token.is_cancelled() {
          Err(ExecutorError::CommandFailed(format!(
            "Task {} failed: {}",
            self.name, stderr
          )))
        } else {
          Ok(stdout.trim().to_string())
        }
      },
      Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(ExecutorError::TaskCancelled(self.name.clone())),
      Err(e) => Err(ExecutorError::CommandFailed(e.to_string())),
    }
  }

  /// Renders the command template with variables and dependency results
  async fn render_template(
    &self,
    plugin_manager: Arc<PluginManager>,
    template: &str,
    cache: Option<Arc<Mutex<IndexMap<String, CacheItem>>>>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    // Interpolate vars
    let mut vars = self.vars.clone();
    vars.expand(dry).await?;

    // Interpolate environment variables
    let mut envs = self.envs.clone();
    envs.expand().await?;

    // Add dependency results to template context
    let deps_res = self.deps_res.lock().await;
    vars.insert("deps_result", &*deps_res);

    let dir = self.interpolate_dir(self.dir.clone(), dry).await?;
    let dir = canonicalize(dir)?;

    match self
      .execute_plugin_command(
        plugin_manager,
        "tpl",
        template.to_owned(),
        vec![],
        dir,
        vars.to_hashmap(),
        self.envs.clone().into(),
        true,
        cancel_token.clone(),
      )
      .await
    {
      Ok((code, stdout, stderr)) => {
        if code != 0 && !cancel_token.is_cancelled() {
          Err(ExecutorError::CommandFailed(format!(
            "Task {} failed: {}",
            self.name, stderr
          )))
        } else {
          if let Some(ref cache) = cache {
            self.update_cache(stdout.trim(), vars, cache).await?;
          }

          Ok(stdout.trim().to_string())
        }
      },
      Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(ExecutorError::TaskCancelled(self.name.clone())),
      Err(e) => Err(ExecutorError::CommandFailed(e.to_string())),
    }
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
    plugin_manager: Arc<PluginManager>,
    cmd: String,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    let mut vars = self.vars.clone();
    vars.expand(dry).await?;

    match self.execute_command(plugin_manager, &cmd, dry, cancel_token).await {
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

  async fn check_preconditions(&self) -> ExecutorResult<bool> {
    let mut tera = Tera::default();
    let mut vars = self.vars.clone();
    vars.expand(false).await?;

    let mut context: Context = vars.into();
    // Add dependency results to template context
    let deps_res = self.deps_res.lock().await;
    context.insert("deps_result", &*deps_res);

    let mut result = true;

    if let Some(preconditions) = &self.preconditions {
      for precondition in preconditions {
        let rendered = tera
          .render_str(precondition, &context)
          .map_err(|e| ExecutorError::ValueExpandError(precondition.to_owned(), e.to_string()))?;

        result = result && (rendered.trim() == "true" || rendered.trim() == "True" || rendered.trim() == "1");
      }
    }

    Ok(result)
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
      if self.run_mode == RunMode::Once {
        return Ok(Some(cached_result.result.clone()));
      } else if &cached_result.vars == vars {
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

  async fn debug_log_dependencies(&self) {
    if enabled!(Level::DEBUG) {
      let deps = self.deps_res.lock().await;
      for (name, res) in &*deps {
        debug!("Dependency {} results: {}", name, res);
      }
    }
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

  async fn bypass_result(&self, result: HashMap<String, String>) {
    let mut deps_res = self.deps_res.lock().await;
    *deps_res = result
  }

  /// Executes the task and returns the result
  async fn execute(
    &self,
    plugin_manager: Arc<PluginManager>,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    fingerprint: Arc<Db>,
    dry: bool,
    force: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    if !force && !self.check_preconditions().await? {
      self.log_info(format!("Task '{}' preconditions failed", self.name));

      return Err(ExecutorError::TaskCancelled(format!(
        "Task '{}' preconditions failed",
        self.name
      )));
    }

    if !force && !self.check_sources(fingerprint.clone()).await? {
      self.log_info(format!("Task {} are up to date", self.name));

      return Ok("".to_string());
    };

    // Interpolate vars
    let mut vars = self.vars.clone();
    vars.expand(dry).await?;

    if let Some(cached) = self.check_cache(&vars, &cache).await? {
      return Ok(cached);
    }

    self.log_info(format!("Starting task {}", self.name));

    // Debug information about dependency results
    self.debug_log_dependencies().await;

    match (&self.extra.get("shell"), self.extra.get("tpl")) {
      // This variant should validate on load octafile stage
      (Some(_), Some(_)) => {
        unreachable!("Both cmd and tpl cannot be Some - should be validated during octafile loading")
      },
      (Some(cmd), None) => {
        self
          .execute_cmd(
            plugin_manager,
            cmd.to_string().trim_matches('"').to_string(),
            cache,
            dry,
            cancel_token.clone(),
          )
          .await
      },
      (None, Some(tpl)) => {
        let rendered_cmd = self
          .render_template(
            plugin_manager,
            tpl.to_string().trim_matches('"'),
            Some(cache),
            dry,
            cancel_token.clone(),
          )
          .await?;

        Ok(rendered_cmd)
      },
      // This variant just if we want only run deps
      (None, None) => Ok("".to_string()),
    }
  }
}

#[async_trait]
impl Identifiable for TaskNode {
  fn id(&self) -> String {
    self.id.clone()
  }

  fn name(&self) -> String {
    self.name.clone()
  }

  async fn get_deps_result(&self) -> HashMap<String, String> {
    let res = self.deps_res.lock().await;
    res.clone()
  }

  fn is_internal(&self) -> bool {
    self.cmd_type == CmdType::Internal
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
  use std::{fs, time::Duration};
  use tempfile::TempDir;

  // Helper function to create a test TaskNode
  fn create_test_task(name: &str, cmd: Option<&str>, tpl: Option<String>, run_mode: Option<RunMode>) -> TaskNode {
    let mut extra = HashMap::new();
    if let Some(tpl) = tpl {
      let tpl_value = Value::String(tpl);
      extra.insert("tpl".to_owned(), tpl_value);
    }
    if let Some(cmd) = cmd {
      let cmd_value = Value::String(cmd.to_owned());
      extra.insert("shell".to_owned(), cmd_value);
    }

    let task_config = TaskConfig::builder()
      .id(name.to_string())
      .name(name.to_string())
      .dep_name(name.to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .extra(extra)
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

    let task = create_test_task("test_task", Some("echo hello world"), None, None);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    let result = task
      .execute(
        plugin_manager.clone(),
        cache,
        fingerprint,
        false,
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result.trim(), "hello world");
    plugin_manager.shutdown_all().await;
  }

  #[tokio::test]
  async fn test_template_rendering() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let mut vars = Vars::new();
    vars.insert(&"name", &"world");

    let mut extra = HashMap::new();
    let tpl_value = Value::String("Hello {{ name }}!".into());
    extra.insert("tpl".to_owned(), tpl_value);

    let task_config = TaskConfig::builder()
      .id("template_task".to_string())
      .name("template_task".to_string())
      .dep_name("template_task".to_string())
      .dir(PathBuf::from("."))
      .vars(vars)
      .envs(Envs::new())
      .extra(extra)
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    let result = task
      .execute(
        plugin_manager.clone(),
        cache,
        fingerprint,
        false,
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result, "Hello world!");
    plugin_manager.shutdown_all().await;
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
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    // First execution
    let result1 = task
      .execute(
        plugin_manager.clone(),
        cache.clone(),
        fingerprint.clone(),
        false,
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result1.trim(), "cached result");

    // Second execution should return cached result
    let result2 = task
      .execute(
        plugin_manager.clone(),
        cache.clone(),
        fingerprint.clone(),
        false,
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result1, result2);
    plugin_manager.shutdown_all().await;
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

    let mut extra = HashMap::new();
    let cmd_value = Value::String("echo 'test'".to_string());
    extra.insert("shell".to_owned(), cmd_value);

    let task_config = TaskConfig::builder()
      .id("source_task".to_string())
      .name("source_task".to_string())
      .dep_name("source_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .extra(extra)
      .run_mode(Some(AllowedRun::Changed))
      .sources(Some(vec![test_file.to_str().unwrap().to_string()]))
      .source_strategy(Some(SourceStrategies::Hash))
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    // First execution
    let result1 = task
      .execute(
        plugin_manager.clone(),
        cache.clone(),
        fingerprint.clone(),
        false,
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
        plugin_manager.clone(),
        cache.clone(),
        fingerprint.clone(),
        false,
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result1, result2);
    plugin_manager.shutdown_all().await;
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
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    let result = task
      .execute(
        plugin_manager.clone(),
        cache,
        fingerprint,
        false,
        false,
        CancellationToken::new(),
      )
      .await;
    assert!(matches!(result, Err(ExecutorError::TaskFailed(_))));
    plugin_manager.shutdown_all().await;
  }

  #[tokio::test]
  async fn test_task_cancellation() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let cancel_token = CancellationToken::new();
    let mut extra = HashMap::new();
    let cmd_value = Value::String("sleep 5".to_string());
    extra.insert("shell".to_owned(), cmd_value);

    let task_config = TaskConfig::builder()
      .id("long_task".to_string())
      .name("long_task".to_string())
      .dep_name("long_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .extra(extra)
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Cancel the task after a short delay
    let cancel_handle = tokio::spawn({
      let cancel_token = cancel_token.clone();
      async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_token.cancel();
      }
    });

    let result = task
      .execute(plugin_manager.clone(), cache, fingerprint, false, false, cancel_token)
      .await;
    assert!(matches!(result, Err(ExecutorError::TaskCancelled(_))));

    cancel_handle.await.unwrap();
    plugin_manager.shutdown_all().await;
  }

  #[tokio::test]
  async fn test_ignore_errors() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let mut extra = HashMap::new();
    let cmd_value = Value::String("nonexistent_command".to_string());
    extra.insert("shell".to_owned(), cmd_value);

    let task_config = TaskConfig::builder()
      .id("ignore_error_task".to_string())
      .name("ignore_error_task".to_string())
      .dep_name("ignore_error_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .extra(extra)
      .ignore_errors(Some(true))
      .build()
      .unwrap();

    let task = TaskNode::new(task_config);

    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let fingerprint = Arc::new(db);
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    let result = task
      .execute(
        plugin_manager.clone(),
        cache,
        fingerprint,
        false,
        false,
        CancellationToken::new(),
      )
      .await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "");
    plugin_manager.shutdown_all().await;
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
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_tpl";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_tpl.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    let result = task
      .execute(
        plugin_manager.clone(),
        cache,
        fingerprint,
        false,
        false,
        CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(result, "Result: dep_output");
    plugin_manager.shutdown_all().await;
  }
}
