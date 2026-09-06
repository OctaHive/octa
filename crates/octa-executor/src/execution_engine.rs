//! Public embedding facade for preparing and starting an execution.
//!
//! The engine owns long-lived services such as plugins, fingerprints, and
//! runtime coordination. Per-run options stay in [`ExecutionRequest`], while a
//! prepared plan can be started immediately or attached to an external parent
//! cancellation token.

use std::{fmt, path::PathBuf, sync::Arc};

use indexmap::IndexMap;
use octa_octafile::{Octafile, Silence};
use octa_output::{Console, ConsoleScopeAllocator, ConsoleStatus};
use octa_plugin_manager::plugin_manager::PluginManager;
use sled::Db;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{
  error::{ExecutorError, ExecutorResult},
  execution_handle::ExecutionHandle,
  execution_result::{conclusion, ExecutionFailure, ExecutionResult},
  execution_run::ExecutionRun,
  executor::{Executor, ExecutorConfig},
  runtime_coordinator::RuntimeCoordinator,
  summary::Summary,
  task::{TaskNode, TaskRuntime},
  vars::VariableResolver,
  RawTerminalConnector, TaskGraphBuilder, UnsupportedRawTerminal,
};

/// Everything that varies between invocations of a prepared execution engine.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ExecutionRequest {
  /// Task command to execute.
  pub command: String,
  /// Directory used to resolve a bare command in monorepo mode.
  pub working_directory: Option<PathBuf>,
  /// Ordered command-line variable overrides.
  pub variables: Vec<(String, String)>,
  /// Arguments exposed to task templates and commands.
  pub command_args: Vec<String>,
  /// Whether independent DAG nodes may execute concurrently.
  pub parallel: bool,
  /// Whether plugins should describe work without mutating the workspace.
  pub dry: bool,
  /// Whether freshness checks should be bypassed.
  pub force: bool,
  /// Whether the first task failure should cancel the remaining plan.
  pub failfast: bool,
  /// Whether informational task diagnostics should be suppressed.
  pub quiet: bool,
  /// Optional task stream suppression override.
  pub silence: Option<Silence>,
  /// Whether the selected command should own a raw terminal session.
  pub raw: bool,
}

impl ExecutionRequest {
  /// Creates a request with headless, serial execution defaults.
  pub fn new(command: impl Into<String>) -> Self {
    Self {
      command: command.into(),
      working_directory: None,
      variables: Vec::new(),
      command_args: Vec::new(),
      parallel: false,
      dry: false,
      force: false,
      failfast: false,
      quiet: false,
      silence: None,
      raw: false,
    }
  }
}

/// Reusable dependencies for building and starting independent task executions.
#[derive(Clone)]
pub struct ExecutionEngine {
  plugin_manager: Arc<PluginManager>,
  octafile: Arc<Octafile>,
  fingerprint: Arc<Db>,
  console: Arc<Console>,
  concurrency: Option<Arc<Semaphore>>,
  variable_resolver: Option<Arc<dyn VariableResolver>>,
  summary: Option<Arc<Summary>>,
  scope_allocator: Arc<ConsoleScopeAllocator>,
  runtime_coordinator: Arc<RuntimeCoordinator>,
  terminal: Arc<dyn RawTerminalConnector>,
}

#[derive(Clone, Copy)]
enum PreparationMode<'a> {
  Inspectable,
  Immediate(&'a CancellationToken),
}

impl ExecutionEngine {
  /// Creates an engine from application-owned runtime services.
  pub fn new(
    plugin_manager: Arc<PluginManager>,
    octafile: Arc<Octafile>,
    fingerprint: Arc<Db>,
    console: Arc<Console>,
  ) -> Self {
    Self {
      plugin_manager,
      octafile,
      fingerprint,
      console,
      concurrency: None,
      variable_resolver: None,
      summary: None,
      scope_allocator: Arc::new(ConsoleScopeAllocator::default()),
      runtime_coordinator: Arc::new(RuntimeCoordinator::default()),
      terminal: Arc::new(UnsupportedRawTerminal),
    }
  }

  /// Shares one concurrency budget between executions created by this engine.
  pub fn with_concurrency(mut self, concurrency: Arc<Semaphore>) -> Self {
    self.concurrency = Some(concurrency);
    self
  }

  /// Supplies an application-owned provider for required variable prompts.
  pub fn with_variable_resolver(mut self, variable_resolver: Arc<dyn VariableResolver>) -> Self {
    self.variable_resolver = Some(variable_resolver);
    self
  }

  /// Aggregates task timings into an application-owned batch summary.
  pub fn with_summary(mut self, summary: Arc<Summary>) -> Self {
    self.summary = Some(summary);
    self
  }

  /// Shares interactive-execution coordination between concurrently running requests.
  pub fn with_runtime_coordinator(mut self, runtime_coordinator: Arc<RuntimeCoordinator>) -> Self {
    self.runtime_coordinator = runtime_coordinator;
    self
  }

  /// Connects raw plugin sessions to a terminal owned by the embedding application.
  pub fn with_raw_terminal(mut self, terminal: Arc<dyn RawTerminalConnector>) -> Self {
    self.terminal = terminal;
    self
  }

  /// Builds an execution without starting it, for ordered batch declaration and watch discovery.
  pub async fn prepare(&self, request: ExecutionRequest) -> ExecutorResult<PreparedExecution> {
    let run_id = self.console.allocate_run_id();
    let run = Arc::new(ExecutionRun::new(self.console.clone(), run_id));
    self
      .prepare_with_run_id(request, run_id, PreparationMode::Inspectable, run)
      .await
  }

  /// Builds and starts an execution on the current Tokio runtime.
  pub fn start(&self, request: ExecutionRequest) -> ExecutionHandle {
    self.spawn(request, CancellationToken::new())
  }

  /// Builds and starts an execution below an application-owned cancellation token.
  pub fn start_with_token(
    &self,
    parent_cancellation: &CancellationToken,
    request: ExecutionRequest,
  ) -> ExecutionHandle {
    self.spawn(request, parent_cancellation.child_token())
  }

  fn spawn(&self, request: ExecutionRequest, cancellation: CancellationToken) -> ExecutionHandle {
    let run_id = self.console.allocate_run_id();
    let command = request.command.clone();
    let engine = self.clone();
    let execution_cancellation = cancellation.clone();
    let task = tokio::spawn(async move { engine.execute_request(request, run_id, execution_cancellation).await });
    ExecutionHandle::new(run_id, command, cancellation, task)
  }

  async fn execute_request(
    &self,
    request: ExecutionRequest,
    run_id: u64,
    cancellation: CancellationToken,
  ) -> ExecutorResult<ExecutionResult> {
    let command = request.command.clone();
    let run = Arc::new(ExecutionRun::new(self.console.clone(), run_id));
    let started_at = run.start(&command).await?;

    let prepared = self
      .prepare_with_run_id(request, run_id, PreparationMode::Immediate(&cancellation), run.clone())
      .await;
    let prepared = match prepared {
      Ok(prepared) => prepared,
      Err(error) => {
        let status = if matches!(error, ExecutorError::TaskCancelled(_)) {
          ConsoleStatus::Cancelled
        } else {
          ConsoleStatus::Failed
        };
        let finished_at = run.finish(&command, status).await?;
        return Ok(ExecutionResult {
          run_id,
          command,
          started_at,
          finished_at,
          conclusion: conclusion(status, Some(ExecutionFailure::from_error(&error, None)), None, None),
          tasks: Vec::new(),
          outputs: Vec::new(),
        });
      },
    };

    let result = prepared.executor.execute(cancellation, &command).await;
    if result.is_err() {
      let _ = run.finish(&command, ConsoleStatus::Failed).await;
    }
    result
  }

  async fn prepare_with_run_id(
    &self,
    request: ExecutionRequest,
    run_id: u64,
    mode: PreparationMode<'_>,
    run: Arc<ExecutionRun>,
  ) -> ExecutorResult<PreparedExecution> {
    let ExecutionRequest {
      command,
      working_directory,
      variables,
      command_args,
      parallel,
      dry,
      force,
      failfast,
      quiet,
      silence,
      raw,
    } = request;
    let parallel = parallel && !raw;
    let mut builder = TaskGraphBuilder::new(self.plugin_manager.clone())?
      .with_scope_allocator(self.scope_allocator.clone())
      .with_output_overrides(quiet, silence, raw)
      .with_variable_overrides(variables);
    if let Some(directory) = working_directory {
      builder = builder.with_working_directory(directory);
    }
    if let Some(resolver) = &self.variable_resolver {
      builder = builder.with_variable_resolver(resolver.clone());
    }
    let build = builder.build(self.octafile.clone(), &command, parallel, command_args);
    let plan = match &mode {
      PreparationMode::Immediate(cancellation) => {
        tokio::select! {
          biased;
          _ = cancellation.cancelled() => return Err(ExecutorError::TaskCancelled(command)),
          result = build => result?,
        }
      },
      PreparationMode::Inspectable => build.await?,
    };
    let is_linear = plan.is_linear()?;
    let watch_targets = if matches!(mode, PreparationMode::Inspectable) {
      plan.nodes().iter().filter_map(|node| node.watch_target()).collect()
    } else {
      Vec::new()
    };
    let executor = Executor::new(
      plan,
      ExecutorConfig {
        failfast,
        concurrency: self.concurrency.clone(),
        run: Some(run),
        runtime_coordinator: self.runtime_coordinator.clone(),
        summary: self.summary.clone(),
      },
      TaskRuntime {
        plugin_manager: self.plugin_manager.clone(),
        terminal: self.terminal.clone(),
        cache: Arc::new(Mutex::new(IndexMap::new())),
        fingerprint: self.fingerprint.clone(),
        console: self.console.clone(),
        run_id,
        dry,
        force,
        deferred_exit_code: None,
      },
    )?;
    Ok(PreparedExecution {
      executor,
      command,
      is_linear,
      watch_targets,
    })
  }
}

impl fmt::Debug for ExecutionEngine {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("ExecutionEngine").finish_non_exhaustive()
  }
}

/// A built execution retained separately from its eventual scheduling strategy.
pub struct PreparedExecution {
  executor: Executor<TaskNode>,
  command: String,
  is_linear: bool,
  watch_targets: Vec<crate::watcher::WatchTarget>,
}

impl PreparedExecution {
  /// Returns the command captured by this plan.
  pub fn command(&self) -> &str {
    &self.command
  }

  /// Returns whether dependency structure alone guarantees serial scheduling.
  pub fn is_linear(&self) -> bool {
    self.is_linear
  }

  /// Returns source groups that should trigger a rebuild in watch mode.
  pub fn watch_targets(&self) -> &[crate::watcher::WatchTarget] {
    &self.watch_targets
  }

  /// Publishes run and task declarations before a later batch execution.
  pub async fn declare(&self) -> ExecutorResult<()> {
    self.executor.prepare(&self.command).await
  }

  /// Executes the prepared plan using the supplied cancellation token.
  pub async fn execute(self, cancellation: CancellationToken) -> ExecutorResult<ExecutionResult> {
    self.executor.execute(cancellation, &self.command).await
  }

  /// Starts the prepared plan on the current Tokio runtime.
  pub fn start(self) -> ExecutionHandle {
    self.executor.start(self.command)
  }

  /// Starts the plan with a child token linked to application-owned cancellation.
  pub fn start_with_token(self, parent_cancellation: &CancellationToken) -> ExecutionHandle {
    self.executor.start_with_token(parent_cancellation, self.command)
  }
}

#[cfg(test)]
mod tests {
  use std::{fs, io, sync::Mutex};

  use octa_output::{ConsoleEntry, ConsoleRecord, ConsoleRenderer, ExecutionEvent};
  use tempfile::TempDir;

  use super::*;
  use crate::ExecutionConclusion;

  #[derive(Clone, Default)]
  struct RecordingRenderer(Arc<Mutex<Vec<ConsoleRecord>>>);

  impl ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.lock().unwrap().push(entry.record().clone());
      Ok(())
    }
  }

  fn test_engine() -> (TempDir, ExecutionEngine, Arc<Console>, RecordingRenderer) {
    let directory = TempDir::new().unwrap();
    let octafile_path = directory.path().join("Octafile.yml");
    fs::write(
      &octafile_path,
      r#"
        version: 1
        tasks:
          build:
            platforms: [unsupported]
            shell: echo build
      "#,
    )
    .unwrap();
    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_owned()], "shell").unwrap();
    let plugin_manager = Arc::new(PluginManager::new(directory.path()));
    let fingerprint = Arc::new(sled::Config::new().temporary(true).open().unwrap());
    let renderer = RecordingRenderer::default();
    let console = Arc::new(Console::new(renderer.clone()));
    let engine = ExecutionEngine::new(plugin_manager, octafile, fingerprint, console.clone());
    (directory, engine, console, renderer)
  }

  #[test]
  fn request_defaults_are_safe_for_headless_serial_execution() {
    let request = ExecutionRequest::new("build");
    assert_eq!(request.command, "build");
    assert!(request.working_directory.is_none());
    assert!(request.variables.is_empty());
    assert!(request.command_args.is_empty());
    assert!(!request.parallel);
    assert!(!request.dry);
    assert!(!request.force);
    assert!(!request.failfast);
    assert!(!request.quiet);
    assert!(request.silence.is_none());
    assert!(!request.raw);
  }

  #[tokio::test]
  async fn prepares_execution_metadata_for_batch_or_watch_callers() {
    let (directory, engine, _console, _renderer) = test_engine();
    let mut request = ExecutionRequest::new("build");
    request.parallel = true;
    request.working_directory = Some(directory.path().to_path_buf());

    let execution = engine.prepare(request).await.unwrap();

    assert_eq!(execution.command(), "build");
    assert!(execution.is_linear());
    assert!(execution.watch_targets().is_empty());
  }

  #[tokio::test]
  async fn starts_with_a_complete_run_lifecycle_and_terminal_result() {
    let (_directory, engine, console, renderer) = test_engine();
    let handle = engine.start(ExecutionRequest::new("build"));
    let run_id = handle.run_id();

    let result = handle.wait().await.unwrap();
    console.drain().await.unwrap();

    assert_eq!(result.run_id, run_id);
    assert!(result.is_success());
    let status = result.conclusion.status();
    let records = renderer.0.lock().unwrap();
    assert!(matches!(
      records.first(),
      Some(ConsoleRecord::Execution(ExecutionEvent::RunStarted { run_id: actual, .. })) if *actual == run_id
    ));
    assert!(matches!(
      records.last(),
      Some(ConsoleRecord::Execution(ExecutionEvent::RunFinished {
        run_id: actual,
        status: actual_status,
        ..
      })) if *actual == run_id && *actual_status == status
    ));
  }

  #[tokio::test]
  async fn cancellation_during_preparation_returns_a_structured_result() {
    let (_directory, engine, _console, _renderer) = test_engine();
    let parent = CancellationToken::new();
    parent.cancel();
    let handle = engine.start_with_token(&parent, ExecutionRequest::new("build"));

    let result = handle.wait().await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Cancelled(_)));
  }

  #[tokio::test]
  async fn prepared_execution_exposes_both_handle_entry_points() {
    let (_directory, engine, _console, _renderer) = test_engine();
    assert_eq!(format!("{engine:?}"), "ExecutionEngine { .. }");

    let result = engine
      .prepare(ExecutionRequest::new("build"))
      .await
      .unwrap()
      .start()
      .wait()
      .await
      .unwrap();
    assert!(result.is_success());

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = engine
      .prepare(ExecutionRequest::new("build"))
      .await
      .unwrap()
      .start_with_token(&cancellation)
      .wait()
      .await
      .unwrap();
    assert!(matches!(result.conclusion, ExecutionConclusion::Cancelled(_)));
  }

  #[tokio::test]
  async fn preparation_failure_is_returned_as_a_terminal_result() {
    let (_directory, engine, _console, _renderer) = test_engine();

    let result = engine.start(ExecutionRequest::new("missing")).wait().await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Failed(_)));
    assert!(result.tasks.is_empty());
  }

  #[tokio::test]
  async fn headless_engine_rejects_raw_execution_without_a_terminal_connector() {
    let directory = TempDir::new().unwrap();
    let octafile_path = directory.path().join("Octafile.yml");
    fs::write(
      &octafile_path,
      r#"
        version: 1
        tasks:
          interactive:
            shell: printf raw
      "#,
    )
    .unwrap();
    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_owned()], "shell").unwrap();
    let project_root = env!("CARGO_MANIFEST_DIR");
    let plugin_manager = Arc::new(PluginManager::new(format!("{project_root}/../../plugins")));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();
    let engine = ExecutionEngine::new(
      plugin_manager.clone(),
      octafile,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      Arc::new(Console::default()),
    );
    let mut request = ExecutionRequest::new("interactive");
    request.raw = true;

    let result = engine.start(request).wait().await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Failed(_)));
    assert!(result
      .failure()
      .is_some_and(|failure| failure.message.contains("host terminal connector")));
    plugin_manager.shutdown_all().await;
  }
}
