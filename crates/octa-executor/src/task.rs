use std::{
  collections::HashMap,
  env,
  hash::{Hash, Hasher},
  io,
  path::{Path, PathBuf},
  sync::{Arc, OnceLock},
  time::Duration,
};

use async_trait::async_trait;
use dunce::canonicalize;
use indexmap::IndexMap;
use octa_plugin::protocol::PluginResponse;
use octa_plugin_manager::plugin_manager::PluginManager;
use serde_json::Value;
use sled::Db;
use tera::{Context, Tera};
use tokio::{sync::Mutex, time};
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, error, info, Level};

use octa_dag::Identifiable;
use octa_octafile::{AllowedRun, SourceStrategies, Timeout};

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
  async fn is_changed(&self, sources: Vec<PathBuf>) -> ExecutorResult<bool>;
}

pub trait TaskItem {
  fn run_mode(&self) -> RunMode;
  fn failfast(&self) -> bool;
  fn requires_concurrency_permit(&self) -> bool;
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum NodeKind {
  #[default]
  Command,
  Barrier,
  Condition,
}

type RuntimeContext = (Vars, Envs, PathBuf);

/// Shared result of a task-level condition evaluated by a condition gate node.
#[derive(Debug, Clone)]
struct ConditionOutcome {
  passed: bool,
  runtime_context: Option<RuntimeContext>,
}

#[derive(Debug, Default)]
pub(crate) struct ConditionState(OnceLock<ConditionOutcome>);

impl ConditionState {
  fn set(&self, passed: bool, runtime_context: Option<RuntimeContext>) {
    let _ = self.0.set(ConditionOutcome {
      passed,
      runtime_context,
    });
  }

  fn passed(&self) -> Option<bool> {
    self.0.get().map(|outcome| outcome.passed)
  }

  fn runtime_context(&self) -> Option<RuntimeContext> {
    self.0.get().and_then(|outcome| outcome.runtime_context.clone())
  }
}

#[derive(Debug, Clone, Default)]
enum SharedConditionState {
  #[default]
  None,
  Publish(Arc<ConditionState>),
  Reuse(Arc<ConditionState>),
}

/// A normalized plugin invocation shared by task commands and conditions.
#[derive(Debug, Clone)]
pub(crate) struct PluginInvocation {
  key: String,
  value: Value,
}

impl PluginInvocation {
  pub(crate) fn new(key: String, value: Value) -> Self {
    Self { key, value }
  }

  fn command(&self) -> String {
    match &self.value {
      Value::String(command) => command.clone(),
      value => value.to_string(),
    }
  }
}

/// Conditions and shared gate state attached to one executable graph node.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConditionRuntime {
  conditions: Vec<PluginInvocation>,
  guards: Vec<Arc<ConditionState>>,
  shared_state: SharedConditionState,
}

impl ConditionRuntime {
  pub(crate) fn command(
    conditions: Vec<PluginInvocation>,
    guards: Vec<Arc<ConditionState>>,
    runtime_context: Option<Arc<ConditionState>>,
  ) -> Self {
    Self {
      conditions,
      guards,
      shared_state: runtime_context.map_or(SharedConditionState::None, SharedConditionState::Reuse),
    }
  }

  pub(crate) fn gate(
    condition: PluginInvocation,
    state: Arc<ConditionState>,
    guards: Vec<Arc<ConditionState>>,
  ) -> Self {
    Self {
      conditions: vec![condition],
      guards,
      shared_state: SharedConditionState::Publish(state),
    }
  }

  fn conditions(&self) -> &[PluginInvocation] {
    &self.conditions
  }

  fn should_run(&self, task_name: &str) -> ExecutorResult<bool> {
    for guard in &self.guards {
      match guard.passed() {
        Some(true) => {},
        Some(false) => {
          self.publish(false, None);
          return Ok(false);
        },
        None => {
          return Err(ExecutorError::TaskFailed(format!(
            "Task '{task_name}' reached an unevaluated condition gate"
          )));
        },
      }
    }

    Ok(true)
  }

  fn publish(&self, passed: bool, runtime_context: Option<RuntimeContext>) {
    if let SharedConditionState::Publish(state) = &self.shared_state {
      state.set(passed, runtime_context);
    }
  }

  fn shared_runtime_context(&self) -> Option<RuntimeContext> {
    match &self.shared_state {
      SharedConditionState::Reuse(state) => state.runtime_context(),
      SharedConditionState::None | SharedConditionState::Publish(_) => None,
    }
  }
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
  pub failfast: bool,      // Cancel parallel work after the first failure

  // Runtime behavior
  pub run_mode: RunMode,                  // Run mode
  pub vars: Vars,                         // Task variables
  pub envs: Envs,                         // Task environments
  pub sources: Option<Vec<String>>,       // Sources for fingerprinting
  pub octafile_root: PathBuf,             // Directory containing the root Octafile
  pub source_strategy: SourceMethod,      // Source validation strategy
  condition_runtime: ConditionRuntime,    // Conditions attached to this graph node
  pub preconditions: Option<Vec<String>>, // Task preconditions
  pub timeout: Option<Timeout>,           // Maximum task execution time

  // State management
  node_kind: NodeKind,
  plugin: Option<PluginInvocation>,
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
  pub failfast: Option<bool>,

  pub run_mode: Option<RunMode>,
  pub vars: Option<Vars>,
  pub envs: Option<Envs>,
  pub sources: Option<Vec<String>>,
  pub octafile_root: Option<PathBuf>,
  pub source_strategy: Option<SourceMethod>,
  condition_runtime: ConditionRuntime,
  pub preconditions: Option<Vec<String>>,
  pub timeout: Option<Timeout>,

  node_kind: NodeKind,
  plugin: Option<PluginInvocation>,
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

  pub fn octafile_root(mut self, octafile_root: impl Into<PathBuf>) -> Self {
    self.octafile_root = Some(octafile_root.into());
    self
  }

  pub fn preconditions(mut self, preconditions: Option<Vec<String>>) -> Self {
    self.preconditions = preconditions;
    self
  }

  pub(crate) fn condition_runtime(mut self, condition_runtime: ConditionRuntime) -> Self {
    self.condition_runtime = condition_runtime;
    self
  }

  pub fn timeout(mut self, timeout: Option<Timeout>) -> Self {
    self.timeout = timeout;
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

  pub(crate) fn plugin(mut self, plugin: Option<PluginInvocation>) -> Self {
    self.plugin = plugin;
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

  pub fn failfast(mut self, failfast: Option<bool>) -> Self {
    self.failfast = failfast;
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

  pub(crate) fn node_kind(mut self, node_kind: NodeKind) -> Self {
    self.node_kind = node_kind;
    self
  }

  pub fn build(self) -> Result<TaskConfig, &'static str> {
    let dir = match self.node_kind {
      NodeKind::Command => self.dir,
      NodeKind::Barrier | NodeKind::Condition => self.dir.or_else(|| Some(env::current_dir().unwrap())),
    };

    let dir = dir.ok_or("Missing mandatory field: dir")?;
    let octafile_root = self.octafile_root.unwrap_or_else(|| dir.clone());

    Ok(TaskConfig {
      id: self.id.ok_or("Missing mandatory field: id")?,
      name: self.name.ok_or("Missing mandatory field: name")?,
      dep_name: self.dep_name.ok_or("Missing mandatory field: dep_name")?,
      dir,
      ignore_errors: self.ignore_errors.unwrap_or(false),
      silent: self.silent.unwrap_or(false),
      failfast: self.failfast.unwrap_or(false),
      run_mode: self.run_mode.unwrap_or(RunMode::Always),
      vars: self.vars.unwrap_or_default(),
      envs: self.envs.unwrap_or_default(),
      sources: self.sources,
      octafile_root,
      condition_runtime: self.condition_runtime,
      preconditions: self.preconditions,
      timeout: self.timeout,
      source_strategy: self.source_strategy.unwrap_or(SourceMethod::Hash),
      node_kind: self.node_kind,
      plugin: self.plugin,
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
  pub failfast: bool,      // Cancel parallel work after the first failure

  // Runtime behavior
  pub run_mode: RunMode,                  // Run mode
  pub vars: Vars,                         // Task variables
  pub envs: Envs,                         // Task environments
  pub sources: Option<Vec<String>>,       // Sources for fingerprinting
  pub octafile_root: PathBuf,             // Directory containing the root Octafile
  pub source_strategy: SourceMethod,      // Source validation strategy
  condition_runtime: ConditionRuntime,    // Conditions attached to this graph node
  pub preconditions: Option<Vec<String>>, // Task run preconditions
  pub timeout: Option<Timeout>,           // Maximum task execution time

  // State management
  pub deps_res: Arc<Mutex<HashMap<String, String>>>, // Dependencies results
  node_kind: NodeKind,
  plugin: Option<PluginInvocation>,
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
      octafile_root: config.octafile_root,
      source_strategy: config.source_strategy,
      vars: config.vars,
      envs: config.envs,
      dir: config.dir,
      ignore_errors: config.ignore_errors,
      silent: config.silent,
      failfast: config.failfast,
      deps_res: Arc::new(Mutex::new(HashMap::default())),
      node_kind: config.node_kind,
      condition_runtime: config.condition_runtime,
      preconditions: config.preconditions,
      timeout: config.timeout,
      plugin: config.plugin,
    }
  }

  #[cfg(test)]
  pub(crate) fn conditions(&self) -> Vec<String> {
    self
      .condition_runtime
      .conditions()
      .iter()
      .map(PluginInvocation::command)
      .collect()
  }

  #[allow(clippy::too_many_arguments)]
  async fn execute_plugin_command(
    &self,
    plugin_manager: Arc<PluginManager>,
    plugin_name: &str,
    dry: bool,
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    secret_vars: Vec<String>,
    silent: bool,
    cancel_token: CancellationToken,
  ) -> io::Result<(i32, String, String)> {
    // Changed return type
    let mut output = String::new();
    let mut errors = String::new();
    let mut exit_code = None;

    let client = plugin_manager.get_client(plugin_name).await.unwrap();
    let mut client_guard = client.lock().await;
    let client = client_guard.as_mut().unwrap();

    // Use a cleanup flag to track if we need to shut down
    let mut needs_cleanup = false;
    let result = async {
      // Start command execution with cancellation support
      let command_id = client
        .execute_with_secrets(
          command.clone(),
          dry,
          args,
          dir,
          vars,
          envs,
          secret_vars,
          cancel_token.clone(),
        )
        .await
        .map_err(io::Error::from)?;

      // Process output until command completes
      loop {
        match client.receive_output(&cancel_token).await {
          // Stdout/stderr belong to the invoked command and remain unchanged; secret metadata
          // protects Octa and plugin diagnostics, not user-controlled command output.
          Ok(Some(response)) => match response {
            PluginResponse::Stdout { id, line } if id == command_id => {
              if !silent {
                println!("{}", line.trim());
              }
              output.push_str(line.trim());
              output.push('\n');
            },
            PluginResponse::Stderr { id, line } if id == command_id => {
              if !silent {
                eprintln!("{}", line.trim());
              }
              errors.push_str(line.trim());
              errors.push('\n');
            },
            PluginResponse::ExitStatus { id, code } if id == command_id => {
              exit_code = Some(code);
              break;
            },
            PluginResponse::Error { id, message } if id == command_id => {
              return Err(io::Error::other(format!("Plugin error: {}", message)));
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
              let _ = client.cancel_and_wait(&command_id).await;
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

  async fn interpolate_dir(&self, dir: PathBuf, vars: &Vars) -> ExecutorResult<PathBuf> {
    let dir_str = dir.to_string_lossy();

    if !dir_str.contains("{{") || !dir_str.contains("}}") {
      debug!("Using direct directory path: {}", dir_str);

      Ok(dir)
    } else {
      debug!("Expanding directory path: {}", dir_str);

      let mut tera = Tera::default();
      let context: Context = vars.clone().into();

      let rendered = tera
        .render_str(&dir_str, &context)
        .map_err(|e| ExecutorError::ValueExpandError(dir_str.to_string(), e.to_string()))?;

      debug!("Expanded path: {}", rendered);

      Ok(PathBuf::from(rendered.trim_matches('"'))) // Remove extra quotes from result
    }
  }

  async fn prepare_dir_with_vars(&self, vars: &Vars, dry: bool) -> ExecutorResult<PathBuf> {
    let dir = self.interpolate_dir(self.dir.clone(), vars).await?;

    self.ensure_dir(dir, dry).await
  }

  async fn ensure_dir(&self, dir: PathBuf, dry: bool) -> ExecutorResult<PathBuf> {
    if dry {
      return match canonicalize(&dir) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
          if dir.is_absolute() {
            Ok(dir)
          } else {
            Ok(env::current_dir()?.join(dir))
          }
        },
        Err(error) => Err(error.into()),
      };
    }

    tokio::fs::create_dir_all(&dir).await?;
    Ok(canonicalize(dir)?)
  }

  /// Resolves the values shared by conditions, cache checks, and the task command once.
  async fn resolve_runtime_context(&self, dry: bool) -> ExecutorResult<(Vars, Envs, PathBuf)> {
    if let Some(context) = self.condition_runtime.shared_runtime_context() {
      return Ok(context);
    }

    let dir_is_template = {
      let value = self.dir.to_string_lossy();
      value.contains("{{") && value.contains("}}")
    };

    // Static task directories must exist before a shell-backed value uses them as its cwd.
    let prepared_dir = if dir_is_template {
      None
    } else {
      Some(self.ensure_dir(self.dir.clone(), dry).await?)
    };

    let mut vars = self.vars.clone();
    vars.expand(dry).await?;

    // A templated directory can only be created after its variables have been expanded.
    let dir = match prepared_dir {
      Some(dir) => dir,
      None => self.prepare_dir_with_vars(&vars, dry).await?,
    };

    let mut envs = self.envs.clone();
    // Task-level shell-backed environment values must run from the final task directory.
    envs.set_dir(dir.clone());
    envs.expand_with(&vars, dry)?;

    Ok((vars, envs, dir))
  }

  fn log_info(&self, message: String) {
    if self.node_kind == NodeKind::Command {
      info!("{}", message);
    }
  }

  async fn check_sources(&self, fingerprint: Arc<Db>) -> ExecutorResult<bool> {
    if let Some(sources) = &self.sources {
      let sources = crate::source::collect(sources, &self.octafile_root)?;
      let strategy: Box<dyn SourceStrategy + Send> = match self.source_strategy {
        SourceMethod::Hash => Box::new(HashSource::new(fingerprint.clone())),
        SourceMethod::Timestamp => Box::new(TimestampSource::new(fingerprint.clone())),
      };
      strategy.is_changed(sources).await
    } else {
      Ok(true)
    }
  }

  async fn check_condition(
    &self,
    plugin_manager: Arc<PluginManager>,
    dry: bool,
    cancel_token: CancellationToken,
    vars: &Vars,
    envs: &Envs,
    dir: &Path,
  ) -> ExecutorResult<bool> {
    if self.condition_runtime.conditions().is_empty() {
      return Ok(true);
    }
    if dry {
      return Ok(true);
    }

    let plugins = plugin_manager.get_schema_keys().await;

    let deps_res = self.deps_res.lock().await;
    let mut vars = vars.clone();
    vars.insert("deps_result", &*deps_res);
    drop(deps_res);

    for condition in self.condition_runtime.conditions() {
      let plugin_name = plugins.get(&condition.key).ok_or(ExecutorError::TaskParsedError)?;
      match self
        .execute_plugin_command(
          plugin_manager.clone(),
          plugin_name,
          dry,
          condition.command(),
          vec![],
          dir.to_path_buf(),
          vars.to_hashmap(),
          envs.clone().into(),
          vars.secret_names(),
          true,
          cancel_token.clone(),
        )
        .await
      {
        Ok((0, _, _)) => {},
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
          return Err(ExecutorError::TaskCancelled(self.name.clone()));
        },
        Err(error) => return Err(error.into()),
      }
    }

    Ok(true)
  }

  async fn check_preconditions(&self, vars: &Vars) -> ExecutorResult<bool> {
    let mut tera = Tera::default();
    let mut context: Context = vars.clone().into();
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

  /// Executes the task without applying its timeout wrapper.
  async fn execute_inner(
    &self,
    plugin_manager: Arc<PluginManager>,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    fingerprint: Arc<Db>,
    dry: bool,
    force: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String> {
    if !self.condition_runtime.should_run(&self.name)? {
      return Ok(String::new());
    }

    let (vars, envs, dir) = self.resolve_runtime_context(dry).await?;
    let condition_passed = self
      .check_condition(plugin_manager.clone(), dry, cancel_token.clone(), &vars, &envs, &dir)
      .await?;
    self
      .condition_runtime
      .publish(condition_passed, Some((vars.clone(), envs.clone(), dir.clone())));
    if !condition_passed {
      self.log_info(format!(
        "Task '{}' skipped because its condition was not met",
        self.name
      ));
      return Ok("".to_string());
    }

    if !force && !self.check_preconditions(&vars).await? {
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

    if let Some(cached) = self.check_cache(&vars, &cache).await? {
      return Ok(cached);
    }

    self.log_info(format!("Starting task {}", self.name));
    self.debug_log_dependencies().await;

    let Some(plugin) = &self.plugin else {
      return Ok("".to_string());
    };
    let plugins = plugin_manager.get_schema_keys().await;
    let plugin_name = plugins.get(&plugin.key).ok_or(ExecutorError::TaskParsedError)?;

    let mut vars_with_deps_results = vars.clone();
    let deps_res = self.deps_res.lock().await;
    vars_with_deps_results.insert("deps_result", &*deps_res);

    match self
      .execute_plugin_command(
        plugin_manager,
        plugin_name,
        dry,
        plugin.command(),
        vec![],
        dir,
        vars_with_deps_results.to_hashmap(),
        envs.into(),
        vars_with_deps_results.secret_names(),
        self.silent,
        cancel_token.clone(),
      )
      .await
    {
      Ok((code, stdout, stderr)) => {
        if code != 0 && !cancel_token.is_cancelled() {
          if self.ignore_errors {
            error!("Task {} failed but errors ignored. Error code: {}", self.name, code);
            Ok("".to_string())
          } else {
            Err(ExecutorError::TaskFailed(format!(
              "Task {} failed: {}",
              self.name, stderr
            )))
          }
        } else {
          self.update_cache(stdout.trim(), vars, &cache).await?;
          Ok(stdout.trim().to_string())
        }
      },
      Err(error) if error.kind() == io::ErrorKind::Interrupted => Err(ExecutorError::TaskCancelled(self.name.clone())),
      Err(error) => self.handle_execution_error(error),
    }
  }

  /// Handle execution errors
  fn handle_execution_error(&self, error: io::Error) -> ExecutorResult<String> {
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
    let Some(timeout) = self.timeout else {
      return self
        .execute_inner(plugin_manager, cache, fingerprint, dry, force, cancel_token)
        .await;
    };

    // A child token limits cancellation to this command while preserving the caller's token.
    let command_token = cancel_token.child_token();
    let execution = self.execute_inner(plugin_manager, cache, fingerprint, dry, force, command_token.clone());
    tokio::pin!(execution);

    tokio::select! {
      result = &mut execution => result,
      _ = time::sleep(timeout.duration()) => {
        command_token.cancel();
        // Wait for protocol cleanup so the plugin can accept another command immediately.
        let _ = time::timeout(Duration::from_secs(5), &mut execution).await;
        let error = ExecutorError::TaskTimedOut {
          task: self.name.clone(),
          timeout: timeout.to_string(),
        };
        if self.ignore_errors {
          error!("Task {} failed but errors ignored. Error: {}", self.name, error);
          Ok(String::new())
        } else {
          Err(error)
        }
      }
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
    self.node_kind != NodeKind::Command
  }
}

impl TaskItem for TaskNode {
  fn run_mode(&self) -> RunMode {
    self.run_mode.clone()
  }

  fn failfast(&self) -> bool {
    self.failfast
  }

  fn requires_concurrency_permit(&self) -> bool {
    self.node_kind != NodeKind::Barrier
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, time::Duration};
  use tempfile::TempDir;

  fn plugin(key: &str, value: impl Into<Value>) -> Option<PluginInvocation> {
    Some(PluginInvocation::new(key.to_owned(), value.into()))
  }

  // Helper function to create a test TaskNode
  fn create_test_task(name: &str, cmd: Option<&str>, tpl: Option<String>, run_mode: Option<RunMode>) -> TaskNode {
    let plugin = tpl
      .map(|value| PluginInvocation::new("tpl".to_owned(), Value::String(value)))
      .or_else(|| cmd.map(|value| PluginInvocation::new("shell".to_owned(), Value::String(value.to_owned()))));

    let task_config = TaskConfig::builder()
      .id(name.to_string())
      .name(name.to_string())
      .dep_name(name.to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .plugin(plugin)
      .run_mode(Some(run_mode.unwrap_or(RunMode::Always)))
      .build()
      .unwrap();

    TaskNode::new(task_config)
  }

  async fn prepare_dir(task: &TaskNode, dry: bool) -> ExecutorResult<PathBuf> {
    let mut vars = task.vars.clone();
    vars.expand(dry).await?;
    task.prepare_dir_with_vars(&vars, dry).await
  }

  #[tokio::test]
  async fn test_prepare_dir_creates_interpolated_directory() {
    let temp_dir = TempDir::new().unwrap();
    let target = temp_dir.path().join("build").join("generated");
    let mut task = create_test_task("create_dir", None, None, None);
    let mut vars = Vars::new();
    vars.insert("OUTPUT_DIR", &target.to_string_lossy().to_string());
    task.vars = vars;
    task.dir = PathBuf::from("{{ OUTPUT_DIR }}");

    let prepared = prepare_dir(&task, false).await.unwrap();

    assert!(target.is_dir());
    assert_eq!(prepared, canonicalize(target).unwrap());
  }

  #[tokio::test]
  async fn test_prepare_dir_dry_run_with_existing_directory() {
    let temp_dir = TempDir::new().unwrap();
    let mut task = create_test_task("existing_dir", None, None, None);
    task.dir = temp_dir.path().to_path_buf();

    let prepared = prepare_dir(&task, true).await.unwrap();

    assert_eq!(prepared, canonicalize(temp_dir.path()).unwrap());
  }

  #[tokio::test]
  async fn test_prepare_dir_dry_run_with_missing_absolute_directory() {
    let temp_dir = TempDir::new().unwrap();
    let target = temp_dir.path().join("missing");
    let mut task = create_test_task("missing_absolute_dir", None, None, None);
    task.dir = target.clone();

    let prepared = prepare_dir(&task, true).await.unwrap();

    assert_eq!(prepared, target);
    assert!(!prepared.exists());
  }

  #[tokio::test]
  async fn test_prepare_dir_dry_run_with_missing_relative_directory() {
    let current_dir = env::current_dir().unwrap();
    let temp_dir = TempDir::new_in(&current_dir).unwrap();
    let relative = temp_dir.path().strip_prefix(&current_dir).unwrap().join("missing");
    let mut task = create_test_task("missing_relative_dir", None, None, None);
    task.dir = relative.clone();

    let prepared = prepare_dir(&task, true).await.unwrap();

    assert_eq!(prepared, current_dir.join(relative));
    assert!(!prepared.exists());
  }

  #[tokio::test]
  async fn test_prepare_dir_rejects_file_path() {
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let mut task = create_test_task("file_path", None, None, None);
    task.dir = temp_file.path().to_path_buf();

    assert!(matches!(
      prepare_dir(&task, false).await,
      Err(ExecutorError::IoError(_))
    ));
  }

  #[tokio::test]
  async fn test_prepare_dir_propagates_interpolation_error() {
    let mut task = create_test_task("invalid_interpolation", None, None, None);
    task.dir = PathBuf::from("{{ invalid + }}");

    assert!(matches!(
      prepare_dir(&task, false).await,
      Err(ExecutorError::ValueExpandError(_, _))
    ));
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn test_prepare_dir_dry_run_propagates_canonicalize_error() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = TempDir::new().unwrap();
    let locked = temp_dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let mut task = create_test_task("unreadable_dir", None, None, None);
    task.dir = locked.join("child");
    let result = prepare_dir(&task, true).await;

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(result, Err(ExecutorError::IoError(_))));
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
    vars.insert("name", &"world");

    let task_config = TaskConfig::builder()
      .id("template_task".to_string())
      .name("template_task".to_string())
      .dep_name("template_task".to_string())
      .dir(PathBuf::from("."))
      .vars(vars)
      .envs(Envs::new())
      .plugin(plugin("tpl", "Hello {{ name }}!"))
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

    let task_config = TaskConfig::builder()
      .id("source_task".to_string())
      .name("source_task".to_string())
      .dep_name("source_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .plugin(plugin("shell", "echo 'test'"))
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
    let task_config = TaskConfig::builder()
      .id("long_task".to_string())
      .name("long_task".to_string())
      .dep_name("long_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .plugin(plugin("shell", "sleep 5"))
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
  async fn test_task_timeout_stops_command_and_keeps_plugin_reusable() {
    let db = Arc::new(sled::Config::new().temporary(true).open().unwrap());
    let cache = Arc::new(Mutex::new(IndexMap::new()));
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../target/debug", project_root)));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();

    #[cfg(not(windows))]
    let long_command = "sleep 5";
    #[cfg(windows)]
    let long_command = "ping -n 5 127.0.0.1";
    let timeout = serde_yml::from_str::<Timeout>("100ms").unwrap();
    let timed_task = TaskNode::new(
      TaskConfig::builder()
        .id("timed_task")
        .name("timed_task")
        .dep_name("timed_task")
        .dir(".")
        .plugin(plugin("shell", long_command))
        .timeout(Some(timeout))
        .build()
        .unwrap(),
    );

    let result = timed_task
      .execute(
        plugin_manager.clone(),
        cache.clone(),
        db.clone(),
        false,
        false,
        CancellationToken::new(),
      )
      .await;
    assert!(matches!(result, Err(ExecutorError::TaskTimedOut { .. })));

    let next_task = TaskNode::new(
      TaskConfig::builder()
        .id("next_task")
        .name("next_task")
        .dep_name("next_task")
        .dir(".")
        .plugin(plugin("shell", "echo reusable"))
        .build()
        .unwrap(),
    );
    let result = next_task
      .execute(
        plugin_manager.clone(),
        cache,
        db,
        false,
        false,
        CancellationToken::new(),
      )
      .await;

    assert_eq!(result.unwrap(), "reusable");
    plugin_manager.shutdown_all().await;
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
      .dep_name("ignore_error_task".to_string())
      .dir(PathBuf::from("."))
      .vars(Vars::new())
      .envs(Envs::new())
      .plugin(plugin("shell", "nonexistent_command"))
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
