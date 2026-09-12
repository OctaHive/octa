//! Public embedding facade for preparing and starting an execution.
//!
//! The engine owns long-lived services such as plugins, cache hashing, and
//! runtime coordination. Per-run options stay in [`ExecutionRequest`], while a
//! prepared plan can be started immediately or attached to an external parent
//! cancellation token.

use std::{fmt, path::PathBuf, sync::Arc};

use indexmap::IndexMap;
use octa_octafile::{Octafile, Silence};
use octa_output::{Console, ConsoleScopeAllocator, ConsoleStatus};
use octa_plugin_manager::plugin_manager::PluginManager;
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
  /// Whether persistent result lookup and execution-local shortcuts should be bypassed.
  pub force: bool,
  /// Whether the first task failure should cancel the remaining plan.
  pub failfast: bool,
  /// Whether informational task diagnostics should be suppressed.
  pub quiet: bool,
  /// Optional task stream suppression override.
  pub silence: Option<Silence>,
  /// Whether the selected command should own a raw terminal session.
  pub raw: bool,
  /// Probe cache identity and availability without invoking task commands.
  pub cache_probe: bool,
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
      cache_probe: false,
    }
  }
}

/// Reusable dependencies for building and starting independent task executions.
#[derive(Clone)]
pub struct ExecutionEngine {
  plugin_manager: Arc<PluginManager>,
  octafile: Arc<Octafile>,
  result_cache: Option<Arc<crate::result_cache::ResultCache>>,
  input_snapshotter: octa_cache::InputSnapshotter,
  console: Arc<Console>,
  concurrency: Option<Arc<Semaphore>>,
  variable_resolver: Option<Arc<dyn VariableResolver>>,
  secret_session: Option<Arc<crate::SecretSession>>,
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
  pub fn new(plugin_manager: Arc<PluginManager>, octafile: Arc<Octafile>, console: Arc<Console>) -> Self {
    Self {
      plugin_manager,
      octafile,
      result_cache: None,
      input_snapshotter: octa_cache::InputSnapshotter::default(),
      console,
      concurrency: None,
      variable_resolver: None,
      secret_session: None,
      summary: None,
      scope_allocator: Arc::new(ConsoleScopeAllocator::default()),
      runtime_coordinator: Arc::new(RuntimeCoordinator::default()),
      terminal: Arc::new(UnsupportedRawTerminal),
    }
  }

  /// Enables persistent task-result caching and shares its hashing budget with watch mode.
  pub fn with_result_cache(mut self, cache: Arc<crate::result_cache::ResultCache>) -> Self {
    self.input_snapshotter = cache.snapshotter();
    self.result_cache = Some(cache);
    self
  }

  /// Returns the runtime-wide input snapshotter used by cache and watch.
  pub fn input_snapshotter(&self) -> octa_cache::InputSnapshotter {
    self.input_snapshotter.clone()
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

  /// Resolves logical Octafile secret references for every execution owned by this engine.
  pub fn with_secret_session(mut self, secret_session: Arc<crate::SecretSession>) -> Self {
    self.secret_session = Some(secret_session);
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
          stdout: Vec::new(),
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
      cache_probe,
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
    if let Some(session) = &self.secret_session {
      builder = builder.with_secret_session(session.clone());
    }
    let build = builder.build_with_watch_targets(self.octafile.clone(), &command, parallel, command_args);
    let built = match &mode {
      PreparationMode::Immediate(cancellation) => {
        tokio::select! {
          biased;
          _ = cancellation.cancelled() => return Err(ExecutorError::TaskCancelled(command)),
          result = build => result?,
        }
      },
      PreparationMode::Inspectable => build.await?,
    };
    let is_linear = built.plan.is_linear()?;
    let watch_targets = if matches!(mode, PreparationMode::Inspectable) {
      built.watch_targets
    } else {
      Vec::new()
    };
    let executor = Executor::new(
      built.plan,
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
        invocation_results: Arc::new(Mutex::new(IndexMap::new())),
        result_cache: self.result_cache.clone(),
        console: self.console.clone(),
        run_id,
        dry,
        force,
        cache_probe,
        deferred_exit_code: None,
        structured_output_budget: Arc::new(crate::structured_output::StructuredOutputBudget::default()),
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

  use octa_cache::{BundleLimits, LocalCacheConfig, LocalCacheStore, RestoreManager};
  use octa_cache_protocol::{Digest, PlatformArchitecture, PlatformOs, RuntimeIdentity};
  use octa_output::{ConsoleEntry, ConsoleRecord, ConsoleRenderer, ExecutionEvent};
  use tempfile::TempDir;

  use super::*;
  use crate::{ExecutionConclusion, ResultCache};

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
    let renderer = RecordingRenderer::default();
    let console = Arc::new(Console::new(renderer.clone()));
    let engine = ExecutionEngine::new(plugin_manager, octafile, console.clone());
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
    let plugin_manager = Arc::new(PluginManager::new(crate::test_support::plugin_directory()));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();
    let engine = ExecutionEngine::new(plugin_manager.clone(), octafile, Arc::new(Console::default()));
    let mut request = ExecutionRequest::new("interactive");
    request.raw = true;

    let result = engine.start(request).wait().await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Failed(_)));
    assert!(result
      .failure()
      .is_some_and(|failure| failure.message.contains("host terminal connector")));
    plugin_manager.shutdown_all().await;
  }

  #[tokio::test]
  async fn task_cache_restores_deleted_outputs_and_preserves_materialized_files() {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("input.txt"), "input").unwrap();
    let octafile_path = directory.path().join("Octafile.yml");
    fs::write(
      &octafile_path,
      r#"
        version: 1
        tasks:
          build:
            files:
              inputs: [input.txt]
              outputs: [artifact.txt, reports]
            cache: {}
            shell: 'printf run >> runs.txt; printf artifact > artifact.txt; mkdir -p reports; printf "<testsuite/>" > reports/junit.xml; printf cached-stdout'
            artifacts:
              - name: application
                path: artifact.txt
                content_type: text/plain
            reports:
              - name: tests
                path: reports/junit.xml
                format: junit
          skipped:
            if: exit 1
            files:
              inputs: [input.txt]
            cache: {}
            shell: printf should-not-run > skipped.txt
          no-filesystem-output:
            files:
              inputs: []
            cache: {}
            shell: 'printf run >> no-output-runs.txt; printf logical-result'
          secret-input:
            vars:
              TOKEN:
                value: hidden
                secret: true
            files:
              inputs: []
            cache: {}
            shell: printf run >> secret-runs.txt
          deferred-output:
            files:
              inputs: [input.txt]
              outputs: [deferred-output.txt]
            cache: {}
            cmds:
              - shell: 'printf body >> deferred-runs.txt; printf incomplete > deferred-output.txt'
              - defer: 'printf cleanup >> deferred-runs.txt; printf complete > deferred-output.txt'
          failed-deferred-output:
            files:
              inputs: [input.txt]
              outputs: [failed-deferred-output.txt]
            cache: {}
            cmds:
              - shell: 'printf body >> failed-deferred-runs.txt; printf incomplete > failed-deferred-output.txt'
              - defer: 'printf cleanup >> failed-deferred-runs.txt; exit 1'
          command-condition:
            files:
              inputs: []
            cache: {}
            cmds:
              - shell: printf skipped >> condition-runs.txt
                if: exit 1
              - shell: 'printf run >> condition-runs.txt; printf condition-result'
          shell-environment:
            env:
              GENERATED:
                sh: printf generated
            files:
              inputs: []
            cache: {}
            shell: 'printf run >> environment-runs.txt; printf "$GENERATED"'
      "#,
    )
    .unwrap();
    let plugin_manager = Arc::new(PluginManager::new(crate::test_support::plugin_directory()));
    #[cfg(not(windows))]
    let plugin_name = "octa_plugin_shell";
    #[cfg(windows)]
    let plugin_name = "octa_plugin_shell.exe";
    plugin_manager.start_plugin(plugin_name).await.unwrap();
    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_owned()], "shell").unwrap();
    let mut missing_cache_request = ExecutionRequest::new("build");
    missing_cache_request.working_directory = Some(directory.path().to_path_buf());
    let missing_cache = ExecutionEngine::new(plugin_manager.clone(), octafile.clone(), Arc::new(Console::default()))
      .start(missing_cache_request)
      .wait()
      .await
      .unwrap();
    assert!(missing_cache
      .failure()
      .is_some_and(|failure| failure.message.contains("no result cache was configured")));

    let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(directory.path().join("cache"))).unwrap());
    let restore = RestoreManager::open(store.layout_root(), BundleLimits::default()).unwrap();
    let cache = Arc::new(
      ResultCache::new(
        store,
        restore,
        "tests",
        RuntimeIdentity::Native {
          os: current_os(),
          architecture: current_architecture(),
          environment: Digest::blake3(b"test-toolchain"),
        },
      )
      .unwrap(),
    );
    let engine =
      ExecutionEngine::new(plugin_manager.clone(), octafile, Arc::new(Console::default())).with_result_cache(cache);
    let request = || {
      let mut request = ExecutionRequest::new("build");
      request.working_directory = Some(directory.path().to_path_buf());
      request
    };

    let first = engine.start(request()).wait().await.unwrap();
    assert!(first.is_success(), "{:?}", first.failure());
    assert_eq!(first.stdout, ["cached-stdout"]);
    assert_eq!(fs::read_to_string(directory.path().join("runs.txt")).unwrap(), "run");
    assert_eq!(
      fs::read_to_string(directory.path().join("artifact.txt")).unwrap(),
      "artifact"
    );

    fs::remove_file(directory.path().join("artifact.txt")).unwrap();
    let second = engine.start(request()).wait().await.unwrap();
    assert!(second.is_success());
    assert_eq!(second.stdout, ["cached-stdout"]);
    assert_eq!(fs::read_to_string(directory.path().join("runs.txt")).unwrap(), "run");
    assert_eq!(
      fs::read_to_string(directory.path().join("artifact.txt")).unwrap(),
      "artifact"
    );
    assert!(second.tasks.iter().any(|task| {
      task
        .cache
        .as_ref()
        .is_some_and(|cache| cache.status == crate::CacheStatus::Hit)
    }));
    let cached_task = second.tasks.iter().find(|task| task.label == "build").unwrap();
    assert_eq!(cached_task.artifacts.len(), 1);
    assert_eq!(cached_task.reports.len(), 1);

    let modified = fs::metadata(directory.path().join("artifact.txt"))
      .unwrap()
      .modified()
      .unwrap();
    let third = engine.start(request()).wait().await.unwrap();
    assert!(third.is_success());
    assert_eq!(
      fs::metadata(directory.path().join("artifact.txt"))
        .unwrap()
        .modified()
        .unwrap(),
      modified
    );
    let mut skipped_request = ExecutionRequest::new("skipped");
    skipped_request.working_directory = Some(directory.path().to_path_buf());
    let skipped = engine.start(skipped_request).wait().await.unwrap();
    assert!(skipped.is_success(), "{:?}", skipped.failure());
    assert!(!directory.path().join("skipped.txt").exists());

    let mut no_output_request = ExecutionRequest::new("no-filesystem-output");
    no_output_request.working_directory = Some(directory.path().to_path_buf());
    let no_output_first = engine.start(no_output_request.clone()).wait().await.unwrap();
    let no_output_second = engine.start(no_output_request).wait().await.unwrap();
    assert_eq!(no_output_first.stdout, ["logical-result"]);
    assert_eq!(no_output_second.stdout, ["logical-result"]);
    assert_eq!(
      fs::read_to_string(directory.path().join("no-output-runs.txt")).unwrap(),
      "run"
    );

    let mut secret_request = ExecutionRequest::new("secret-input");
    secret_request.working_directory = Some(directory.path().to_path_buf());
    let secret_first = engine.start(secret_request.clone()).wait().await.unwrap();
    let secret_second = engine.start(secret_request).wait().await.unwrap();
    assert!(secret_first.is_success() && secret_second.is_success());
    assert_eq!(
      fs::read_to_string(directory.path().join("secret-runs.txt")).unwrap(),
      "runrun"
    );
    assert!(secret_second.tasks.iter().any(|task| {
      task.cache.as_ref().is_some_and(|cache| {
        cache.status == crate::CacheStatus::Bypassed && cache.reason == Some(octa_output::CacheReason::SecretVariables)
      })
    }));

    let mut deferred_request = ExecutionRequest::new("deferred-output");
    deferred_request.working_directory = Some(directory.path().to_path_buf());
    let deferred_first = engine.start(deferred_request.clone()).wait().await.unwrap();
    assert!(deferred_first.is_success(), "{:?}", deferred_first.failure());
    assert_eq!(
      fs::read_to_string(directory.path().join("deferred-runs.txt")).unwrap(),
      "bodycleanup"
    );
    assert_eq!(
      fs::read_to_string(directory.path().join("deferred-output.txt")).unwrap(),
      "complete"
    );
    fs::remove_file(directory.path().join("deferred-output.txt")).unwrap();
    let deferred_second = engine.start(deferred_request).wait().await.unwrap();
    assert!(deferred_second.is_success(), "{:?}", deferred_second.failure());
    assert_eq!(
      fs::read_to_string(directory.path().join("deferred-runs.txt")).unwrap(),
      "bodycleanup"
    );
    assert_eq!(
      fs::read_to_string(directory.path().join("deferred-output.txt")).unwrap(),
      "complete"
    );

    // Cleanup failure remains non-fatal for the task, but the partially
    // finalized output must never become a reusable action result.
    let mut failed_deferred_request = ExecutionRequest::new("failed-deferred-output");
    failed_deferred_request.working_directory = Some(directory.path().to_path_buf());
    let failed_deferred_first = engine.start(failed_deferred_request.clone()).wait().await.unwrap();
    let failed_deferred_second = engine.start(failed_deferred_request).wait().await.unwrap();
    assert!(failed_deferred_first.is_success() && failed_deferred_second.is_success());
    assert_eq!(
      fs::read_to_string(directory.path().join("failed-deferred-runs.txt")).unwrap(),
      "bodycleanupbodycleanup"
    );
    assert!(failed_deferred_second.tasks.iter().any(|task| {
      task
        .cache
        .as_ref()
        .is_some_and(|cache| cache.reason == Some(octa_output::CacheReason::DeferredFailed))
    }));

    let mut condition_request = ExecutionRequest::new("command-condition");
    condition_request.working_directory = Some(directory.path().to_path_buf());
    let condition_first = engine.start(condition_request.clone()).wait().await.unwrap();
    let condition_second = engine.start(condition_request).wait().await.unwrap();
    assert!(condition_first.is_success() && condition_second.is_success());
    assert_eq!(condition_first.stdout, ["condition-result"]);
    assert_eq!(condition_second.stdout, ["condition-result"]);
    assert_eq!(
      fs::read_to_string(directory.path().join("condition-runs.txt")).unwrap(),
      "run"
    );

    // The shell capability used to resolve an environment value contributes
    // its exact plugin identity even though it is not the task command key.
    let mut environment_request = ExecutionRequest::new("shell-environment");
    environment_request.working_directory = Some(directory.path().to_path_buf());
    let environment_first = engine.start(environment_request.clone()).wait().await.unwrap();
    let environment_second = engine.start(environment_request).wait().await.unwrap();
    assert!(environment_first.is_success() && environment_second.is_success());
    assert_eq!(environment_first.stdout, ["generated"]);
    assert_eq!(environment_second.stdout, ["generated"]);
    assert_eq!(
      fs::read_to_string(directory.path().join("environment-runs.txt")).unwrap(),
      "run"
    );

    let mut dry_request = request();
    dry_request.dry = true;
    assert!(engine.start(dry_request).wait().await.unwrap().is_success());
    assert_eq!(fs::read_to_string(directory.path().join("runs.txt")).unwrap(), "run");

    let mut force_request = request();
    force_request.force = true;
    let forced = engine.start(force_request).wait().await.unwrap();
    assert!(forced.is_success());
    assert_eq!(fs::read_to_string(directory.path().join("runs.txt")).unwrap(), "runrun");
    assert!(forced.tasks.iter().any(|task| {
      task.cache.as_ref().is_some_and(|cache| {
        cache.status == crate::CacheStatus::Miss && cache.reason == Some(octa_output::CacheReason::Force)
      })
    }));
    plugin_manager.shutdown_all().await;
  }

  fn current_os() -> PlatformOs {
    match std::env::consts::OS {
      "linux" => PlatformOs::Linux,
      "windows" => PlatformOs::Windows,
      "macos" => PlatformOs::Macos,
      other => panic!("unsupported test OS {other}"),
    }
  }

  fn current_architecture() -> PlatformArchitecture {
    match std::env::consts::ARCH {
      "x86_64" => PlatformArchitecture::Amd64,
      "aarch64" => PlatformArchitecture::Arm64,
      other => panic!("unsupported test architecture {other}"),
    }
  }
}
