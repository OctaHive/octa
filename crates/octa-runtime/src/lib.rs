//! Shared headless runtime used by both the interactive CLI and `octa-runner`.
//!
//! This crate owns workspace loading, plugin lifecycle, task planning, and
//! execution. Transport and presentation remain in its callers.

#![warn(missing_docs)]

mod cache;

pub use cache::{CacheMode, ConfiguredCache, RuntimeCacheConfig, RuntimeCacheError};

use std::{collections::HashMap, num::NonZeroUsize, path::PathBuf, sync::Arc};

use octa_executor::{
  ExecutionEngine, ExecutionRequest, ExecutionResult, PreparedExecution, RawTerminalConnector, RuntimeCoordinator,
  SecretProfile, SecretSession, Summary, UnsupportedRawTerminal, VariableResolver, WatchTarget,
};
use octa_finder::OctaFinder;
use octa_monorepo::{MonorepoError, MonorepoResolution};
use octa_octafile::{Octafile, OctafileError, PluginTypeSchema, Silence, SyntheticInclude};
use octa_output::{Console, ConsoleLevel};
use octa_plugin::{protocol::Schema, SHELL_CAPABILITY};
use octa_plugin_manager::{
  plugin_lock::{load_plugin_lock, PluginLockError},
  plugin_manager::{PluginManager, PluginManagerError},
};
use thiserror::Error;
use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;

const BUILTIN_PLUGINS: [&str; 2] = ["shell", "tpl"];

/// Failure while loading, executing, or shutting down a workspace runtime.
#[derive(Debug, Error)]
pub enum RuntimeError {
  /// Filesystem or console I/O failed.
  #[error(transparent)]
  Io(#[from] std::io::Error),
  /// Octafile discovery, parsing, or validation failed.
  #[error(transparent)]
  Octafile(#[from] OctafileError),
  /// Monorepo project discovery or composition failed.
  #[error(transparent)]
  Monorepo(#[from] MonorepoError),
  /// Persistent monorepo discovery state could not be opened or updated.
  #[error("monorepo discovery state failed: {0}")]
  MonorepoState(#[source] MonorepoError),
  /// Task planning or execution failed.
  #[error(transparent)]
  Execution(#[from] octa_executor::ExecutorError),
  /// Result-cache profile loading or composition failed.
  #[error(transparent)]
  Cache(#[from] RuntimeCacheError),
  /// Plugin digest-lock loading or verification failed.
  #[error(transparent)]
  PluginLock(Box<PluginLockError>),
  /// Plugin setup is invalid before user code starts.
  #[error("invalid plugin configuration: {0}")]
  PluginManagerConfiguration(#[source] PluginManagerError),
  /// A configured plugin process failed at runtime.
  #[error("plugin runtime failed: {0}")]
  PluginInfrastructure(#[source] PluginManagerError),
  /// A plugin name or default capability cannot be resolved.
  #[error("invalid plugin configuration: {0}")]
  PluginConfiguration(String),
  /// A spawned root execution panicked or was aborted.
  #[error("execution task failed: {0}")]
  Join(#[from] tokio::task::JoinError),
  /// Cancellation stopped runtime bootstrap or execution.
  #[error("execution cancelled")]
  Cancelled,
}

/// Result returned by runtime composition and execution operations.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

impl From<PluginLockError> for RuntimeError {
  fn from(error: PluginLockError) -> Self {
    Self::PluginLock(Box::new(error))
  }
}

/// Concrete configuration needed to load one Octa workspace.
///
/// Paths may be absolute or relative to `workspace`. A runtime starts and owns
/// its plugin processes and must be shut down after the final execution.
pub struct RuntimeConfig {
  /// Workspace from which Octafile and relative paths are resolved.
  pub workspace: PathBuf,
  /// Optional explicit Octafile path.
  pub octafile: Option<PathBuf>,
  /// Whether Octafile discovery uses the global location.
  pub global: bool,
  /// Runtime-owned state directory.
  pub data_dir: PathBuf,
  /// Directory containing plugin manifests and executables.
  pub plugins_dir: PathBuf,
  /// Optional digest lock required for plugin execution.
  pub plugin_lock: Option<PathBuf>,
  /// Optional logical-secret provider profile.
  pub secrets_profile: Option<PathBuf>,
  /// Validated machine-specific result-cache configuration.
  pub result_cache: Option<RuntimeCacheConfig>,
  /// Additional plugins requested by the caller.
  pub plugins: Vec<String>,
  /// Optional default plugin for bare task definitions.
  pub default_plugin: Option<String>,
  /// Workspace-wide public variable overrides.
  pub variables: Vec<(String, String)>,
  /// Optional maximum number of concurrently executing tasks.
  pub concurrency: Option<NonZeroUsize>,
  /// Optional interactive provider for unresolved required variables.
  pub variable_resolver: Option<Arc<dyn VariableResolver>>,
  /// Connector used by raw terminal task execution.
  pub raw_terminal: Arc<dyn RawTerminalConnector>,
  /// Structured event and presentation destination.
  pub console: Arc<Console>,
  /// Parent cancellation token for the runtime lifecycle.
  pub cancellation: CancellationToken,
}

impl RuntimeConfig {
  /// Creates the minimal non-interactive configuration used by a runner.
  pub fn headless(workspace: PathBuf, plugins_dir: PathBuf, data_dir: PathBuf, console: Arc<Console>) -> Self {
    Self {
      workspace,
      octafile: None,
      global: false,
      data_dir,
      plugins_dir,
      plugin_lock: None,
      secrets_profile: None,
      result_cache: None,
      plugins: Vec::new(),
      default_plugin: None,
      variables: Vec::new(),
      concurrency: None,
      variable_resolver: None,
      raw_terminal: Arc::new(UnsupportedRawTerminal),
      console,
      cancellation: CancellationToken::new(),
    }
  }
}

#[derive(Clone, Debug, Default)]
/// Per-invocation execution flags; workspace state lives in [`Runtime`].
pub struct RunOptions {
  /// Allow independent graph nodes to run concurrently.
  pub parallel: bool,
  /// Plan task commands without executing them.
  pub dry: bool,
  /// Ignore cache hits and task-local reuse decisions.
  pub force: bool,
  /// Cancel remaining work after the first task failure.
  pub failfast: bool,
  /// Per-invocation public variable overrides.
  pub variables: Vec<(String, String)>,
  /// Ordered user arguments forwarded to the selected task.
  pub task_args: Vec<String>,
  /// Suppress ordinary Octa diagnostics.
  pub quiet: bool,
  /// Optional task-stream suppression policy.
  pub silence: Option<Silence>,
  /// Request an exclusive raw terminal session.
  pub raw: bool,
  /// Inspect cache identity and availability without executing task commands.
  pub cache_probe: bool,
}

/// Loaded workspace runtime with one plugin and secret-session lifecycle.
pub struct Runtime {
  workspace: PathBuf,
  engine: ExecutionEngine,
  plugin_manager: Arc<PluginManager>,
  secret_session: Option<Arc<SecretSession>>,
  octafile: Arc<Octafile>,
  console: Arc<Console>,
  summary: Arc<Summary>,
  current_namespace: Option<Vec<String>>,
  monorepo_cache_hit: bool,
  monorepo_project_count: usize,
  concurrency_is_one: bool,
  cancellation: CancellationToken,
}

impl Runtime {
  /// Loads the Octafile, starts its plugin set, and prepares shared execution state.
  pub async fn load(config: RuntimeConfig) -> RuntimeResult<Self> {
    let RuntimeConfig {
      workspace,
      octafile,
      global,
      data_dir,
      plugins_dir,
      plugin_lock,
      secrets_profile,
      result_cache,
      plugins,
      default_plugin,
      variables,
      concurrency,
      variable_resolver,
      raw_terminal,
      console,
      cancellation,
    } = config;

    check_cancelled(&cancellation)?;
    // Opening includes an exact bounded scan and crash recovery. Complete it
    // before starting plugins so a cache failure cannot leak child processes
    // from a partially constructed runtime.
    let result_cache = match result_cache {
      Some(config) => Some(config.open().await?),
      None => None,
    };
    let secret_session = secrets_profile
      .map(|path| if path.is_absolute() { path } else { workspace.join(path) })
      .map(|path| SecretProfile::load(&path).and_then(SecretSession::new))
      .transpose()?
      .map(Arc::new);
    let plugin_lock = plugin_lock
      .map(|path| if path.is_absolute() { path } else { workspace.join(path) })
      .map(|path| load_plugin_lock(&path).map_err(RuntimeError::from))
      .transpose()?;
    let locked = plugin_lock.is_some();
    let plugin_manager = Arc::new(match plugin_lock {
      Some(lock) => PluginManager::with_locked_plugins(plugins_dir, &workspace, lock),
      None => PluginManager::with_workspace(plugins_dir, &workspace),
    });
    let schemas = start_plugins(plugin_manager.clone(), plugins, locked, cancellation.clone()).await?;
    check_cancelled(&cancellation)?;
    let default_plugin = resolve_default_plugin(default_plugin, &schemas)?;
    let mut plugin_schemas = HashMap::new();
    for schema in schemas.values() {
      plugin_schemas.insert(
        schema.key.clone(),
        PluginTypeSchema {
          input: schema.input_schema.clone(),
          output: schema.output_schema.clone(),
        },
      );
    }

    let data_dir = if data_dir.is_absolute() {
      data_dir
    } else {
      workspace.join(data_dir)
    };
    check_cancelled(&cancellation)?;
    let entry_path = Octafile::resolve_path(octafile.clone(), global, Some(workspace.clone()))?;
    // Monorepo discovery owns and lazily opens its incremental state. Task
    // result caching has a separate store and never reuses this directory.
    let monorepo = octa_monorepo::resolve(
      &entry_path,
      &workspace,
      octafile.is_some() || global,
      &data_dir.join("monorepo"),
    )
    .map_err(classify_monorepo_error)?;
    check_cancelled(&cancellation)?;
    let synthetic_includes = synthetic_includes(&monorepo);
    let loaded = Octafile::load_with_schemas_vars_and_includes_from(
      Some(monorepo.root_octafile),
      false,
      None,
      plugin_schemas,
      default_plugin,
      &variables,
      &synthetic_includes,
    )?;

    let summary = Arc::new(Summary::new());
    let effective_concurrency = effective_concurrency(concurrency, loaded.concurrency);
    let mut engine = ExecutionEngine::new(plugin_manager.clone(), loaded.clone(), console.clone())
      .with_summary(summary.clone())
      .with_runtime_coordinator(Arc::new(RuntimeCoordinator::default()))
      .with_raw_terminal(raw_terminal);
    if let Some(limit) = effective_concurrency {
      engine = engine.with_concurrency(Arc::new(Semaphore::new(limit.get())));
    }
    if let Some(resolver) = variable_resolver {
      engine = engine.with_variable_resolver(resolver);
    }
    if let Some(session) = &secret_session {
      engine = engine.with_secret_session(session.clone());
    }
    if let Some(cache) = result_cache {
      engine = engine.with_result_cache(cache.result_cache());
    }

    Ok(Self {
      workspace,
      engine,
      plugin_manager,
      secret_session,
      octafile: loaded,
      console,
      summary,
      current_namespace: monorepo.current_namespace,
      monorepo_cache_hit: monorepo.cache_hit,
      monorepo_project_count: monorepo.projects.len(),
      concurrency_is_one: effective_concurrency.is_some_and(|limit| limit.get() == 1),
      cancellation,
    })
  }

  /// Returns the fully composed workspace Octafile.
  pub fn octafile(&self) -> &Arc<Octafile> {
    &self.octafile
  }

  /// Returns the execution summary shared by all prepared commands.
  pub fn summary(&self) -> &Arc<Summary> {
    &self.summary
  }

  /// Returns the runtime's structured output destination.
  pub fn console(&self) -> &Arc<Console> {
    &self.console
  }

  /// Returns a child-capable clone of the runtime cancellation token.
  pub fn cancellation(&self) -> CancellationToken {
    self.cancellation.clone()
  }

  /// Reports whether monorepo discovery reused its metadata cache.
  pub fn monorepo_cache_hit(&self) -> bool {
    self.monorepo_cache_hit
  }

  /// Returns the number of loaded monorepo projects.
  pub fn monorepo_project_count(&self) -> usize {
    self.monorepo_project_count
  }

  /// Qualifies bare task names with the current monorepo namespace.
  pub fn qualify_commands(&self, commands: Vec<String>) -> Vec<String> {
    qualify_commands(commands, self.current_namespace.as_deref())
  }

  /// Returns whether any selected task explicitly enables watch mode.
  pub fn commands_request_watch(&self, commands: &[String]) -> bool {
    let finder = OctaFinder::new();
    commands.iter().any(|command| {
      finder
        .find_by_path(self.octafile.clone(), command)
        .iter()
        .any(|result| result.task.watch.unwrap_or(false))
    })
  }

  /// Builds validated execution plans and their shared watch targets.
  pub async fn prepare(
    &self,
    commands: &[String],
    options: &RunOptions,
  ) -> RuntimeResult<(Vec<PreparedExecution>, Vec<WatchTarget>)> {
    let mut executions = Vec::with_capacity(commands.len());
    let mut watch_targets = Vec::new();
    let mut plan_is_parallel = false;

    for command in commands {
      if !(options.quiet || self.octafile.quiet.unwrap_or(false)) {
        self
          .console
          .message(
            ConsoleLevel::Info,
            format!(
              "Building DAG for command {} with provided args {:?}",
              command, options.task_args
            ),
          )
          .await?;
      }
      let mut request = ExecutionRequest::new(command);
      request.working_directory = Some(self.workspace.clone());
      request.parallel = options.parallel;
      request.dry = options.dry;
      request.force = options.force;
      request.failfast = options.failfast;
      request.variables = options.variables.clone();
      request.command_args = options.task_args.clone();
      request.quiet = options.quiet;
      request.silence = options.silence;
      request.raw = options.raw;
      request.cache_probe = options.cache_probe;
      let execution = self.engine.prepare(request).await?;

      plan_is_parallel |= options.parallel || !execution.is_linear();
      watch_targets.extend_from_slice(execution.watch_targets());
      executions.push(execution);
    }

    self
      .console
      .set_parallel(plan_is_parallel && !options.raw && !self.concurrency_is_one)
      .await?;
    Ok((executions, watch_targets))
  }

  /// Returns the hashing service shared by prepared cache actions and watch mode.
  pub fn input_snapshotter(&self) -> octa_cache::InputSnapshotter {
    self.engine.input_snapshotter()
  }

  /// Executes prepared roots serially or concurrently and returns terminal results.
  pub async fn execute(
    &self,
    executions: Vec<PreparedExecution>,
    parallel: bool,
    failfast: bool,
  ) -> RuntimeResult<Vec<ExecutionResult>> {
    let batch_token = self.cancellation.child_token();
    if !parallel {
      let mut results = Vec::with_capacity(executions.len());
      for execution in executions {
        let result = execution.execute(batch_token.clone()).await?;
        let failed = !result.is_success();
        results.push(result);
        if failed {
          break;
        }
      }
      return Ok(results);
    }

    for execution in &executions {
      execution.declare().await?;
    }
    let result_count = executions.len();
    let mut tasks = JoinSet::new();
    for (index, execution) in executions.into_iter().enumerate() {
      let task_token = batch_token.clone();
      tasks.spawn(async move { (index, execution.execute(task_token).await) });
    }

    let mut results = vec![None; result_count];
    let mut first_error = None;
    while let Some(joined) = tasks.join_next().await {
      match joined {
        Ok((index, Ok(result))) => {
          if failfast && !result.is_success() {
            batch_token.cancel();
          }
          results[index] = Some(result);
        },
        Ok((_, Err(error))) => {
          if failfast {
            batch_token.cancel();
          }
          first_error.get_or_insert(RuntimeError::Execution(error));
        },
        Err(error) => {
          if failfast {
            batch_token.cancel();
          }
          first_error.get_or_insert(RuntimeError::Join(error));
        },
      }
    }
    if let Some(error) = first_error {
      return Err(error);
    }
    Ok(results.into_iter().flatten().collect())
  }

  /// Revokes session-owned Vault tokens and gracefully terminates every plugin.
  pub async fn shutdown(&self) {
    if let Some(session) = &self.secret_session {
      session.shutdown().await;
    }
    for error in self
      .plugin_manager
      .shutdown_all()
      .await
      .into_iter()
      .filter_map(Result::err)
    {
      let _ = self
        .console
        .message(ConsoleLevel::Error, format!("Failed to shut down plugin: {error}"))
        .await;
    }
  }
}

fn synthetic_includes(monorepo: &MonorepoResolution) -> Vec<SyntheticInclude> {
  monorepo
    .projects
    .iter()
    .map(|project| SyntheticInclude {
      namespace: project.namespace.clone(),
      path: project.octafile.clone(),
    })
    .collect()
}

fn classify_monorepo_error(error: MonorepoError) -> RuntimeError {
  match error {
    MonorepoError::Cache(_) | MonorepoError::CacheEncoding(_) => RuntimeError::MonorepoState(error),
    _ => RuntimeError::Monorepo(error),
  }
}

fn effective_concurrency(requested: Option<NonZeroUsize>, configured: Option<NonZeroUsize>) -> Option<NonZeroUsize> {
  requested.or(configured)
}

fn qualify_commands(commands: Vec<String>, namespace: Option<&[String]>) -> Vec<String> {
  let Some(namespace) = namespace.filter(|namespace| !namespace.is_empty()) else {
    return commands;
  };
  let prefix = namespace.join(":");
  commands
    .into_iter()
    .map(|command| {
      if command.contains(':') {
        command
      } else {
        format!("{prefix}:{command}")
      }
    })
    .collect()
}

async fn start_plugins(
  manager: Arc<PluginManager>,
  configured: Vec<String>,
  locked: bool,
  cancellation: CancellationToken,
) -> RuntimeResult<HashMap<String, Schema>> {
  let mut tasks = JoinSet::new();
  let mut plugin_names = configured
    .into_iter()
    .chain(BUILTIN_PLUGINS.into_iter().map(str::to_owned))
    .collect::<std::collections::HashSet<_>>()
    .into_iter()
    .collect::<Vec<_>>();
  plugin_names.sort();
  for plugin in plugin_names {
    let manager = manager.clone();
    let cancellation = cancellation.clone();
    tasks.spawn(async move {
      #[cfg(not(windows))]
      let executable = format!("octa_plugin_{plugin}");
      #[cfg(windows)]
      let executable = format!("octa_plugin_{plugin}.exe");
      let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(RuntimeError::Cancelled),
        result = async {
          if locked {
            manager.start_locked_plugin(&plugin).await
          } else {
            manager.start_plugin(&executable).await
          }
        } => result,
      };
      let schema = match result {
        Ok(schema) => schema,
        Err(error) => return Err(classify_plugin_error(error)),
      };
      Ok((plugin, schema))
    });
  }

  let mut schemas = HashMap::new();
  while let Some(result) = tasks.join_next().await {
    let (name, schema) = result??;
    schemas.insert(name, schema);
  }
  Ok(schemas)
}

fn check_cancelled(cancellation: &CancellationToken) -> RuntimeResult<()> {
  if cancellation.is_cancelled() {
    Err(RuntimeError::Cancelled)
  } else {
    Ok(())
  }
}

fn classify_plugin_error(error: PluginManagerError) -> RuntimeError {
  match error {
    PluginManagerError::PluginNotFound(_)
    | PluginManagerError::PluginAlreadyRunning(_)
    | PluginManagerError::PluginSelectorAlreadyRegistered(_)
    | PluginManagerError::Lock(_) => RuntimeError::PluginManagerConfiguration(error),
    PluginManagerError::StartError(_)
    | PluginManagerError::IdentityError(_)
    | PluginManagerError::ShutdownError(_)
    | PluginManagerError::ConnectionError(_)
    | PluginManagerError::Io(_)
    | PluginManagerError::Launch(_)
    | PluginManagerError::SocketPath(_)
    | PluginManagerError::PipeError(_) => RuntimeError::PluginInfrastructure(error),
  }
}

fn resolve_default_plugin(configured: Option<String>, schemas: &HashMap<String, Schema>) -> RuntimeResult<String> {
  if let Some(key) = configured {
    if schemas.values().any(|schema| schema.key == key) {
      return Ok(key);
    }
    return Err(RuntimeError::PluginConfiguration(format!(
      "unknown default plugin task type '{key}'"
    )));
  }

  schemas
    .values()
    .find(|schema| {
      schema
        .capabilities
        .iter()
        .any(|capability| capability == SHELL_CAPABILITY)
    })
    .or_else(|| schemas.get("shell"))
    .map(|schema| schema.key.clone())
    .ok_or_else(|| RuntimeError::PluginConfiguration("no plugin provides the shell capability".to_owned()))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn schema(key: &str, capabilities: &[&str]) -> Schema {
    Schema {
      key: key.to_owned(),
      supports_raw: false,
      capabilities: capabilities.iter().map(|value| (*value).to_owned()).collect(),
      input_schema: None,
      output_schema: None,
    }
  }

  #[test]
  fn requested_concurrency_overrides_the_octafile_default() {
    let configured = NonZeroUsize::new(2);
    let requested = NonZeroUsize::new(5);
    assert_eq!(effective_concurrency(requested, configured), requested);
    assert_eq!(effective_concurrency(None, configured), configured);
  }

  #[test]
  fn qualifies_only_bare_commands_inside_a_monorepo_project() {
    let namespace = ["services".to_owned(), "api".to_owned()];
    assert_eq!(
      qualify_commands(vec!["build".to_owned(), "shared:test".to_owned()], Some(&namespace)),
      ["services:api:build", "shared:test"]
    );
    assert_eq!(qualify_commands(vec!["build".to_owned()], None), ["build"]);
  }

  #[test]
  fn resolves_the_configured_default_plugin_task_type() {
    let schemas = HashMap::from([
      ("shell".to_owned(), schema("shell", &[SHELL_CAPABILITY])),
      ("custom".to_owned(), schema("command", &[])),
    ]);
    assert_eq!(
      resolve_default_plugin(Some("command".to_owned()), &schemas).unwrap(),
      "command"
    );
    assert!(matches!(
      resolve_default_plugin(Some("missing".to_owned()), &schemas),
      Err(RuntimeError::PluginConfiguration(_))
    ));
  }

  #[test]
  fn discovers_a_shell_default_by_capability_and_reports_its_absence() {
    let schemas = HashMap::from([("custom".to_owned(), schema("command", &[SHELL_CAPABILITY]))]);
    assert_eq!(resolve_default_plugin(None, &schemas).unwrap(), "command");
    assert!(matches!(
      resolve_default_plugin(None, &HashMap::new()),
      Err(RuntimeError::PluginConfiguration(_))
    ));
  }

  #[test]
  fn separates_plugin_configuration_from_runtime_failures() {
    assert!(matches!(
      classify_plugin_error(PluginManagerError::PluginNotFound("missing".to_owned())),
      RuntimeError::PluginManagerConfiguration(_)
    ));
    assert!(matches!(
      classify_plugin_error(PluginManagerError::ConnectionError("closed".to_owned())),
      RuntimeError::PluginInfrastructure(_)
    ));
    assert!(matches!(
      classify_plugin_error(PluginManagerError::IdentityError("changed".to_owned())),
      RuntimeError::PluginInfrastructure(_)
    ));
  }

  #[test]
  fn separates_monorepo_configuration_from_state_failures() {
    assert!(matches!(
      classify_monorepo_error(MonorepoError::InvalidConfiguration("roots".to_owned())),
      RuntimeError::Monorepo(_)
    ));
    let encoding = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
    assert!(matches!(
      classify_monorepo_error(MonorepoError::CacheEncoding(encoding)),
      RuntimeError::MonorepoState(_)
    ));
  }

  #[test]
  fn preserves_plugin_lock_errors_at_the_runtime_boundary() {
    let error = RuntimeError::from(PluginLockError::MissingPlugin("junit".to_owned()));
    assert!(matches!(
      error,
      RuntimeError::PluginLock(error) if matches!(*error, PluginLockError::MissingPlugin(ref name) if name == "junit")
    ));
  }

  #[test]
  fn detects_pre_cancelled_runtime_loading() {
    let cancellation = CancellationToken::new();
    assert!(check_cancelled(&cancellation).is_ok());
    cancellation.cancel();
    assert!(matches!(check_cancelled(&cancellation), Err(RuntimeError::Cancelled)));
  }
}
