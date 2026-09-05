use std::{
  cmp::Reverse,
  collections::{HashMap, HashSet},
  hash::Hash,
  ops::Deref,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  },
  time::{Duration, SystemTime},
};

use futures::future::join_all;
use indexmap::IndexMap;
use octa_dag::{Identifiable, DAG};
use octa_output::{Console, ConsoleLevel, ConsoleScope, ConsoleStatus, ExecutionEvent};
use octa_plugin_manager::plugin_manager::PluginManager;
use sled::Db;
use tokio::{
  select,
  sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
  task::JoinHandle,
  time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info_span, Instrument};

use crate::{
  console_scope_tracker::ConsoleScopeTracker,
  error::{ExecutorError, ExecutorResult},
  interactive_scope_tracker::InteractiveScopeTracker,
  runtime_coordinator::RuntimeCoordinator,
  summary::{Summary, TaskSummaryItem},
  task::{CacheItem, Executable, TaskItem, TaskOutcome, TaskRuntime},
};

// Add shutdown timeout constant
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Keeps the first useful failure while allowing an originating error to replace a secondary
/// cancellation reported by a fail-fast sibling.
fn record_execution_error(recorded: &mut Option<ExecutorError>, error: ExecutorError) {
  let recorded_is_cancellation = matches!(recorded.as_ref(), Some(ExecutorError::TaskCancelled(_)));
  let error_is_cancellation = matches!(&error, ExecutorError::TaskCancelled(_));

  if recorded.is_none() || recorded_is_cancellation && !error_is_cancellation {
    *recorded = Some(error);
  }
}

/// A task graph together with cleanup actions managed by the executor.
///
/// Deferred actions are intentionally kept outside `TaskNode`: task nodes describe work,
/// while the execution plan describes when additional work must run.
#[derive(Clone)]
pub struct ExecutionPlan<T: Eq + Hash + Identifiable> {
  /// Main task graph. Deferred actions are represented here only by internal barrier nodes.
  dag: DAG<T>,

  /// Cleanup actions indexed by the ID of their corresponding barrier node.
  deferred: HashMap<String, Arc<DeferredAction<T>>>,

  /// Invocation scopes declared while expanding this plan.
  scopes: Vec<ConsoleScope>,
}

/// A cleanup action attached to an internal barrier in the main task graph.
#[derive(Clone)]
pub(crate) struct DeferredAction<T: Eq + Hash + Identifiable> {
  /// Name used in executor logs for the nested cleanup plan.
  pub(crate) command: String,

  /// Nested plan that executes the shell, task reference, or plugin command.
  pub(crate) plan: ExecutionPlan<T>,

  /// Declaration order used to run unfinished cleanup actions in LIFO order.
  pub(crate) order: usize,

  /// Tasks that must finish before this cleanup action is considered registered.
  pub(crate) registered_after: Vec<String>,
}

impl<T: Eq + Hash + Identifiable> ExecutionPlan<T> {
  pub(crate) fn new(dag: DAG<T>, deferred: HashMap<String, Arc<DeferredAction<T>>>, scopes: Vec<ConsoleScope>) -> Self {
    Self { dag, deferred, scopes }
  }

  /// Whether the scheduler can have more than one graph node ready at once.
  pub fn is_linear(&self) -> ExecutorResult<bool> {
    self.dag.is_linear().map_err(Into::into)
  }
}

impl<T: Eq + Hash + Identifiable> From<DAG<T>> for ExecutionPlan<T> {
  /// Keeps plain DAG execution available for callers that do not use deferred actions.
  fn from(dag: DAG<T>) -> Self {
    Self {
      dag,
      deferred: HashMap::new(),
      scopes: Vec::new(),
    }
  }
}

impl<T: Eq + Hash + Identifiable> Deref for ExecutionPlan<T> {
  type Target = DAG<T>;

  fn deref(&self) -> &Self::Target {
    // Graph inspection remains transparent for existing builder consumers.
    &self.dag
  }
}

/// Configuration for the Executor
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
  /// Emit top-level run lifecycle events. Stream suppression belongs to task configuration.
  pub emit_run_events: bool,
  /// Cancel tasks that are already running when any task in the plan fails.
  pub failfast: bool,
  /// Shared limiter for executable work; graph barriers do not consume permits.
  pub concurrency: Option<Arc<Semaphore>>,
  /// Presentation sink shared by the top-level and deferred execution plans.
  pub console: Arc<Console>,
  /// Existing run identity inherited by nested deferred plans.
  pub run_id: Option<u64>,
  /// Isolation shared by independently built plans in one execution batch.
  pub runtime_coordinator: Arc<RuntimeCoordinator>,
}

impl Default for ExecutorConfig {
  fn default() -> Self {
    Self {
      emit_run_events: false,
      failfast: false,
      concurrency: None,
      console: Arc::new(Console::default()),
      run_id: None,
      runtime_coordinator: Arc::new(RuntimeCoordinator::default()),
    }
  }
}

/// Tracks the state of task execution
#[derive(Debug)]
struct ExecutionState<T: Hash + Identifiable + Eq + TaskItem> {
  dag: Arc<DAG<T>>,                               // Task dependency graph
  in_degree: Arc<Mutex<HashMap<String, usize>>>,  // Tracks task dependencies
  active_tasks: Arc<AtomicUsize>,                 // Number of running tasks
  summary: Arc<Summary>,                          // Summary of task execution
  cache: Arc<Mutex<IndexMap<String, CacheItem>>>, // Cache for tasks
  fingerprint: Arc<Db>,                           // Fingerprint db
  dry: bool,                                      // Dry mode
  force: bool,
  // Successful nodes determine which deferred actions were registered before an interruption.
  completed_tasks: Arc<Mutex<HashSet<String>>>,
}

/// Executor manages the execution of tasks in a directed acyclic graph (DAG)
pub struct Executor<T: Eq + Hash + Identifiable + TaskItem + Executable<T> + Send + Sync + Clone + 'static> {
  state: ExecutionState<T>,
  deferred: Arc<HashMap<String, Arc<DeferredAction<T>>>>,
  config: ExecutorConfig,
  finished: CancellationToken,
  plugin_manager: Arc<PluginManager>,
  scope_tracker: Arc<ConsoleScopeTracker>,
  interactive_tracker: Arc<InteractiveScopeTracker>,
  run_id: u64,
  deferred_exit_code: Option<i32>,
}

#[allow(clippy::too_many_arguments)]
impl<T: Eq + Hash + Identifiable + TaskItem + Executable<T> + Send + Sync + Clone + 'static> Executor<T> {
  /// Creates a new Executor instance with the given DAG
  pub fn new(
    plugin_manager: Arc<PluginManager>,
    plan: impl Into<ExecutionPlan<T>>,
    config: ExecutorConfig,
    cache: Option<Arc<Mutex<IndexMap<String, CacheItem>>>>,
    fingerprint: Arc<Db>,
    dry: bool,
    force: bool,
    summary: Option<Arc<Summary>>,
  ) -> ExecutorResult<Self> {
    let plan = plan.into();
    let dag = plan.dag;
    let node_scopes = dag
      .nodes()
      .iter()
      .filter_map(|node| node.output_scope())
      .collect::<Vec<_>>();
    let run_id = config.run_id.unwrap_or_else(|| config.console.allocate_run_id());
    let scope_tracker = Arc::new(ConsoleScopeTracker::new(
      config.console.clone(),
      run_id,
      plan.scopes,
      node_scopes,
    ));
    let interactive_tracker = Arc::new(InteractiveScopeTracker::new(
      config.runtime_coordinator.clone(),
      dag.nodes().iter().map(|node| {
        (
          node.interactive_session().map(str::to_owned),
          node.requires_runtime_lock(),
        )
      }),
    ));
    let in_degree = dag.nodes().iter().map(|n| (n.id().clone(), 0)).collect();

    let cache = match cache {
      Some(cache) => cache,
      None => Arc::new(Mutex::new(IndexMap::new())),
    };

    let summary = summary.unwrap_or(Arc::new(Summary::new()));

    let state = ExecutionState {
      dag: Arc::new(dag),
      in_degree: Arc::new(Mutex::new(in_degree)),
      active_tasks: Arc::new(AtomicUsize::new(0)),
      summary,
      cache,
      dry,
      force,
      completed_tasks: Arc::new(Mutex::new(HashSet::new())),
      fingerprint,
    };

    Ok(Self {
      state,
      deferred: Arc::new(plan.deferred),
      config,
      finished: CancellationToken::new(),
      plugin_manager,
      scope_tracker,
      interactive_tracker,
      run_id,
      deferred_exit_code: None,
    })
  }

  /// Executes all tasks in the DAG
  pub async fn execute(&self, cancel_token: CancellationToken, command: &str) -> ExecutorResult<Vec<String>> {
    if self.config.emit_run_events {
      self
        .config
        .console
        .event(ExecutionEvent::RunStarted {
          run_id: self.run_id,
          command: command.to_owned(),
        })
        .await?;
    }
    if let Err(error) = self.scope_tracker.declare().await {
      let error = ExecutorError::from(error);
      let _ = self.scope_tracker.finish_remaining(ConsoleStatus::Failed).await;
      if self.config.emit_run_events {
        let _ = self
          .config
          .console
          .event(ExecutionEvent::RunFinished {
            run_id: self.run_id,
            command: command.to_owned(),
            status: ConsoleStatus::Failed,
          })
          .await;
      }
      return Err(error);
    }
    let result = self.execute_plan(cancel_token).await;
    let deferred_exit_code = result.as_ref().err().and_then(ExecutorError::command_exit_code);
    self.interactive_tracker.finish_remaining().await;
    self.run_deferred(deferred_exit_code).await;

    let mut status = match &result {
      Ok(_) => self.scope_tracker.successful_run_status().await,
      Err(ExecutorError::TaskCancelled(_)) => ConsoleStatus::Cancelled,
      Err(_) => ConsoleStatus::Failed,
    };
    let finish_result = self.scope_tracker.finish_remaining(status).await;
    if finish_result.is_err() {
      status = ConsoleStatus::Failed;
    }
    let run_result = if self.config.emit_run_events {
      self
        .config
        .console
        .event(ExecutionEvent::RunFinished {
          run_id: self.run_id,
          command: command.to_owned(),
          status,
        })
        .await
    } else {
      Ok(())
    };
    match result {
      Err(error) => Err(error),
      Ok(values) => {
        finish_result?;
        run_result?;
        Ok(values)
      },
    }
  }

  async fn execute_plan(&self, cancel_token: CancellationToken) -> ExecutorResult<Vec<String>> {
    self.initialize_execution().await?;
    let (tx, rx) = self.create_task_channel();
    let mut handles = Vec::with_capacity(self.state.dag.node_count());
    // Internal cancellation must not cancel the caller's token: the caller may reuse it for
    // another top-level task or a subsequent watch iteration.
    let execution_token = cancel_token.child_token();

    self.schedule_initial_tasks(&tx).await?;

    match self.process_tasks(execution_token.clone(), rx, &tx, &mut handles).await {
      Ok(_) => self.handle_completion(cancel_token, handles).await,
      Err(error) => {
        execution_token.cancel();
        Err(error)
      },
    }
  }

  async fn initialize_execution(&self) -> ExecutorResult<()> {
    self.initialize_in_degrees().await
  }

  fn create_task_channel(&self) -> (mpsc::Sender<Arc<T>>, mpsc::Receiver<Arc<T>>) {
    mpsc::channel(self.state.dag.node_count())
  }

  async fn run_deferred(&self, exit_code: Option<i32>) {
    let completed_tasks = self.state.completed_tasks.lock().await.clone();
    let mut deferred = self
      .deferred
      .iter()
      .map(|(id, action)| (id, action.clone()))
      .collect::<Vec<_>>();
    deferred.sort_by_key(|(_, action)| Reverse(action.order));

    for (id, action) in deferred {
      // A completed barrier means the action already ran during normal DAG execution.
      if completed_tasks.contains(id) {
        continue;
      }

      // An action declared after the failed node was never reached and must not run.
      if !action
        .registered_after
        .iter()
        .all(|task_id| completed_tasks.contains(task_id))
      {
        continue;
      }

      if let Err(error) = execute_deferred_action(
        action,
        TaskRuntime {
          plugin_manager: self.plugin_manager.clone(),
          cache: self.state.cache.clone(),
          fingerprint: self.state.fingerprint.clone(),
          console: self.config.console.clone(),
          run_id: self.run_id,
          dry: self.state.dry,
          force: self.state.force,
          deferred_exit_code: exit_code,
        },
        self.config.concurrency.clone(),
        self.config.runtime_coordinator.clone(),
      )
      .await
      {
        let _ = self
          .config
          .console
          .run_message(
            self.run_id,
            ConsoleLevel::Error,
            format!("Deferred command failed: {error}"),
          )
          .await;
      }
    }
  }

  async fn handle_completion(
    &self,
    cancel_token: CancellationToken,
    handles: Vec<JoinHandle<ExecutorResult<String>>>,
  ) -> ExecutorResult<Vec<String>> {
    if cancel_token.is_cancelled() {
      self.shutdown(handles).await
    } else {
      self.complete_execution(handles).await
    }
  }

  /// Processes tasks as they become available
  async fn process_tasks(
    &self,
    cancel_token: CancellationToken,
    mut rx: mpsc::Receiver<Arc<T>>,
    tx: &mpsc::Sender<Arc<T>>,
    handles: &mut Vec<JoinHandle<ExecutorResult<String>>>,
  ) -> ExecutorResult<()> {
    while let Some(task) = self.receive_next_task(&mut rx, &cancel_token).await {
      handles.push(self.spawn_task(cancel_token.clone(), task, tx.clone()));
    }
    Ok(())
  }

  async fn receive_next_task(
    &self,
    rx: &mut mpsc::Receiver<Arc<T>>,
    cancel_token: &CancellationToken,
  ) -> Option<Arc<T>> {
    select! {
        task = rx.recv() => task,
        _ = cancel_token.cancelled() => {
          debug!("Execution cancelled, stop processing task");
          None
        }
        _ = self.finished.cancelled() => None
    }
  }

  /// Spawns a new task execution
  fn spawn_task(
    &self,
    cancel_token: CancellationToken,
    task: Arc<T>,
    tx: mpsc::Sender<Arc<T>>,
  ) -> JoinHandle<ExecutorResult<String>> {
    let executor_state = ExecutorContext {
      dag: self.state.dag.clone(),
      finished: self.finished.clone(),
      in_degree: self.state.in_degree.clone(),
      active_tasks: self.state.active_tasks.clone(),
      summary: self.state.summary.clone(),
      cache: self.state.cache.clone(),
      fingerprint: self.state.fingerprint.clone(),
      dry: self.state.dry,
      force: self.state.force,
      failfast: self.config.failfast,
      concurrency: self.config.concurrency.clone(),
      console: self.config.console.clone(),
      scope_tracker: self.scope_tracker.clone(),
      interactive_tracker: self.interactive_tracker.clone(),
      runtime_coordinator: self.config.runtime_coordinator.clone(),
      run_id: self.run_id,
      completed_tasks: self.state.completed_tasks.clone(),
      deferred: self.deferred.clone(),
      deferred_exit_code: self.deferred_exit_code,
    };

    let plugin_manager = Arc::clone(&self.plugin_manager);

    let run_id = executor_state.run_id;
    // Runtime tracing diagnostics inherit the same correlation id carried by
    // structured task output without coupling the executor to a tracing layer.
    tokio::spawn(
      async move {
        TaskExecutor::new(executor_state, task, tx, cancel_token, plugin_manager)
          .execute()
          .await
      }
      .instrument(info_span!("task_execution", run_id)),
    )
  }

  /// Schedules tasks with no dependencies
  async fn schedule_initial_tasks(&self, tx: &mpsc::Sender<Arc<T>>) -> ExecutorResult<()> {
    let degrees = self.state.in_degree.lock().await;
    let mut scheduled = 0;
    for node in self.state.dag.nodes() {
      // Deferred barriers are released only by their predecessors, never as graph roots.
      if !self.deferred.contains_key(&node.id()) && degrees[&node.id()] == 0 {
        self.state.active_tasks.fetch_add(1, Ordering::SeqCst);
        tx.send(node.clone()).await.map_err(|_| ExecutorError::ChannelError)?;
        scheduled += 1;
      }
    }
    if scheduled == 0 {
      self.finished.cancel();
    }
    Ok(())
  }

  /// Initializes dependency counts for all tasks
  async fn initialize_in_degrees(&self) -> ExecutorResult<()> {
    let mut degrees = self.state.in_degree.lock().await;
    for deps in self.state.dag.edges().values() {
      for node in deps {
        *degrees
          .get_mut(&node.id())
          .ok_or_else(|| ExecutorError::TaskNotFound(node.id().clone()))? += 1;
      }
    }

    Ok(())
  }

  async fn complete_execution(&self, handles: Vec<JoinHandle<ExecutorResult<String>>>) -> ExecutorResult<Vec<String>> {
    let mut results = vec![];
    let mut first_error = None;

    for handle in handles {
      match handle.await {
        Ok(Ok(result)) => results.push(result),
        Ok(Err(error)) => record_execution_error(&mut first_error, error),
        Err(error) => record_execution_error(&mut first_error, ExecutorError::JoinError(error)),
      }
    }

    if let Some(error) = first_error {
      return Err(error);
    }

    Ok(results)
  }

  async fn shutdown(&self, handles: Vec<JoinHandle<ExecutorResult<String>>>) -> ExecutorResult<Vec<String>> {
    self.log_info("Initiating graceful shutdown").await?;

    match timeout(SHUTDOWN_TIMEOUT, join_all(handles)).await {
      Ok(results) => self.handle_shutdown_results(results).await,
      Err(_) => {
        let _ = self.log_error("Shutdown timeout exceeded, forcing shutdown").await;
        Err(ExecutorError::ShutdownTimeout)
      },
    }
  }

  async fn handle_shutdown_results(
    &self,
    results: Vec<Result<ExecutorResult<String>, tokio::task::JoinError>>,
  ) -> ExecutorResult<Vec<String>> {
    for result in results {
      if let Err(e) = result.map_err(ExecutorError::JoinError)? {
        self.log_error(&format!("Task failed during shutdown: {e}")).await?;
      }
    }
    self.log_info("Graceful shutdown completed").await?;
    Ok(vec![])
  }

  async fn log_info(&self, message: &str) -> ExecutorResult<()> {
    if self.config.emit_run_events {
      self
        .config
        .console
        .run_message(self.run_id, ConsoleLevel::Info, message)
        .await?;
    }
    Ok(())
  }

  async fn log_error(&self, message: &str) -> ExecutorResult<()> {
    self
      .config
      .console
      .run_message(self.run_id, ConsoleLevel::Error, message)
      .await?;
    Ok(())
  }
}

struct ExecutorContext<T: Hash + Identifiable + Eq> {
  dag: Arc<DAG<T>>,
  finished: CancellationToken,
  in_degree: Arc<Mutex<HashMap<String, usize>>>,
  active_tasks: Arc<AtomicUsize>,
  summary: Arc<Summary>,
  cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
  fingerprint: Arc<Db>,
  dry: bool,
  force: bool,
  failfast: bool,
  concurrency: Option<Arc<Semaphore>>,
  console: Arc<Console>,
  scope_tracker: Arc<ConsoleScopeTracker>,
  interactive_tracker: Arc<InteractiveScopeTracker>,
  runtime_coordinator: Arc<RuntimeCoordinator>,
  run_id: u64,
  completed_tasks: Arc<Mutex<HashSet<String>>>,
  deferred: Arc<HashMap<String, Arc<DeferredAction<T>>>>,
  deferred_exit_code: Option<i32>,
}

struct TaskExecutor<T: Executable<T> + Identifiable + TaskItem + Hash + Eq + Send + Sync + Clone + 'static> {
  context: ExecutorContext<T>,
  task: Arc<T>,
  tx: mpsc::Sender<Arc<T>>,
  cancel_token: CancellationToken,
  plugin_manager: Arc<PluginManager>,
}

impl<T: Executable<T> + Identifiable + TaskItem + Hash + Eq + Send + Sync + Clone + 'static> TaskExecutor<T> {
  fn new(
    context: ExecutorContext<T>,
    task: Arc<T>,
    tx: mpsc::Sender<Arc<T>>,
    cancel_token: CancellationToken,
    plugin_manager: Arc<PluginManager>,
  ) -> Self {
    Self {
      context,
      task,
      tx,
      cancel_token,
      plugin_manager,
    }
  }

  async fn execute(self) -> ExecutorResult<String> {
    let session = self.task.interactive_session().map(str::to_owned);
    let needs_runtime_lock = self.task.requires_runtime_lock();
    let _runtime_guard = self
      .context
      .interactive_tracker
      .enter(session.as_deref(), needs_runtime_lock)
      .await;
    let result = self.execute_locked().await;
    self
      .context
      .interactive_tracker
      .complete(session.as_deref(), needs_runtime_lock)
      .await;
    result
  }

  async fn execute_locked(&self) -> ExecutorResult<String> {
    let task_name = self.task.id();
    debug!("Executing task: {}", task_name);

    if let Some(scope) = self.task.output_scope() {
      if let Err(error) = self.context.scope_tracker.start(&scope).await {
        let result = self.handle_error(error.into()).await;
        return self.complete_scope(result).await;
      }
    }

    // Barriers do not consume capacity. Hidden executable nodes such as conditions do.
    let _permit = match self.acquire_permit().await {
      Ok(permit) => permit,
      Err(error) => {
        let result = self.handle_error(error).await;
        return self.complete_scope(result).await;
      },
    };
    let start_time = SystemTime::now();
    let deferred = self.context.deferred.get(&task_name).cloned();
    let result = if let Some(action) = deferred.as_ref() {
      // The graph node is only an ordering barrier; its actual work lives in the nested plan.
      execute_deferred_action(
        action.clone(),
        TaskRuntime {
          plugin_manager: self.plugin_manager.clone(),
          cache: self.context.cache.clone(),
          fingerprint: self.context.fingerprint.clone(),
          console: self.context.console.clone(),
          run_id: self.context.run_id,
          dry: self.context.dry,
          force: self.context.force,
          deferred_exit_code: self.context.deferred_exit_code,
        },
        self.context.concurrency.clone(),
        self.context.runtime_coordinator.clone(),
      )
      .await
      .map(TaskOutcome::success)
    } else {
      self
        .task
        .execute(
          TaskRuntime {
            plugin_manager: self.plugin_manager.clone(),
            cache: self.context.cache.clone(),
            fingerprint: self.context.fingerprint.clone(),
            console: self.context.console.clone(),
            run_id: self.context.run_id,
            dry: self.context.dry,
            force: self.context.force,
            deferred_exit_code: self.context.deferred_exit_code,
          },
          self.cancel_token.clone(),
        )
        .await
    };

    let result = match result {
      Ok(outcome) => {
        let status = outcome.status();
        self
          .handle_success(outcome.into_output(), status, start_time)
          .await
          .map(|output| TaskOutcome::new(output, status))
      },
      Err(error) if deferred.is_some() => {
        // Cleanup failures are reported without replacing the main task result.
        let _ = self
          .context
          .console
          .run_message(
            self.context.run_id,
            ConsoleLevel::Error,
            format!("Deferred command failed: {error}"),
          )
          .await;
        self
          .handle_success(String::new(), ConsoleStatus::Success, start_time)
          .await
          .map(TaskOutcome::success)
      },
      Err(e) => self.handle_error(e).await,
    };
    self.complete_scope(result).await
  }

  async fn complete_scope(&self, result: ExecutorResult<TaskOutcome>) -> ExecutorResult<String> {
    let Some(scope) = self.task.output_scope() else {
      return result.map(TaskOutcome::into_output);
    };
    let status = match &result {
      Ok(outcome) => outcome.status(),
      Err(ExecutorError::TaskCancelled(_)) => ConsoleStatus::Cancelled,
      Err(_) => ConsoleStatus::Failed,
    };
    let completion = self.context.scope_tracker.complete(&scope, status).await;
    match result {
      Err(error) => Err(error),
      Ok(outcome) => {
        completion?;
        Ok(outcome.into_output())
      },
    }
  }

  async fn acquire_permit(&self) -> ExecutorResult<Option<OwnedSemaphorePermit>> {
    if !self.task.requires_concurrency_permit() {
      return Ok(None);
    }

    let Some(concurrency) = &self.context.concurrency else {
      return Ok(None);
    };

    select! {
      permit = concurrency.clone().acquire_owned() => permit
        .map(Some)
        .map_err(|_| ExecutorError::ConcurrencyLimiterClosed),
      _ = self.cancel_token.cancelled() => Err(ExecutorError::TaskCancelled(self.task.name())),
    }
  }

  async fn handle_success(
    &self,
    output: String,
    status: ConsoleStatus,
    start_time: SystemTime,
  ) -> ExecutorResult<String> {
    if self.cancel_token.is_cancelled() {
      debug!("Task {} cancelled during execution", self.task.id());
      return Ok(String::from(""));
    }

    if status == ConsoleStatus::Success && !self.task.is_internal() {
      if let Ok(elapsed) = start_time.elapsed() {
        self
          .context
          .summary
          .add(TaskSummaryItem {
            name: self.task.name(),
            duration: elapsed,
          })
          .await;
      }
    }

    // This also marks deferred barriers as already handled by the normal execution path.
    self.context.completed_tasks.lock().await.insert(self.task.id());

    self.process_task_success(output).await
  }

  async fn handle_error(&self, error: ExecutorError) -> ExecutorResult<TaskOutcome> {
    let message = format!("Task {} failed: {error}", self.task.name());
    let _ = match self.task.output_scope() {
      Some(scope) => {
        self
          .context
          .console
          .run_message_at(self.context.run_id, scope, ConsoleLevel::Error, message)
          .await
      },
      None => {
        self
          .context
          .console
          .run_message(self.context.run_id, ConsoleLevel::Error, message)
          .await
      },
    };
    // `finished` prevents new dependants from being scheduled. The execution token additionally
    // interrupts siblings that were already running when fail-fast behavior is requested.
    if self.context.failfast || self.task.failfast() {
      self.cancel_token.cancel();
    }
    self.context.finished.cancel();
    Err(error)
  }

  async fn process_task_success(&self, output: String) -> ExecutorResult<String> {
    if let Some(deps) = self.context.dag.edges().get(&self.task.id()) {
      for dep in deps {
        if self.task.is_internal() {
          let res = self.task.get_deps_result().await;
          dep.bypass_result(res).await;
        } else {
          dep.set_result(self.task.name(), output.clone()).await;
        }
      }

      let mut degrees = self.context.in_degree.lock().await;
      for dep in deps {
        let dep_count = degrees
          .get_mut(&dep.id())
          .ok_or_else(|| ExecutorError::TaskNotFound(dep.id()))?;
        *dep_count -= 1;

        if *dep_count == 0 && !self.context.finished.is_cancelled() {
          self.context.active_tasks.fetch_add(1, Ordering::SeqCst);
          self
            .tx
            .send(dep.clone())
            .await
            .map_err(|_| ExecutorError::ChannelError)?;
        }
      }
    }

    if self.context.active_tasks.fetch_sub(1, Ordering::SeqCst) == 1 {
      self.context.finished.cancel();
    }

    Ok(output)
  }
}

async fn execute_deferred_action<
  T: Eq + Hash + Identifiable + TaskItem + Executable<T> + Send + Sync + Clone + 'static,
>(
  action: Arc<DeferredAction<T>>,
  runtime: TaskRuntime,
  concurrency: Option<Arc<Semaphore>>,
  runtime_coordinator: Arc<RuntimeCoordinator>,
) -> ExecutorResult<String> {
  let TaskRuntime {
    plugin_manager,
    cache,
    fingerprint,
    console,
    run_id,
    dry,
    force,
    deferred_exit_code,
  } = runtime;
  // A fresh token lets cleanup continue even when cancellation stopped the main plan.
  let mut executor = Executor::new(
    plugin_manager,
    action.plan.clone(),
    ExecutorConfig {
      emit_run_events: false,
      failfast: false,
      concurrency,
      console,
      run_id: Some(run_id),
      runtime_coordinator,
    },
    Some(cache),
    fingerprint,
    dry,
    force,
    None,
  )?;
  executor.deferred_exit_code = deferred_exit_code;

  Box::pin(executor.execute(CancellationToken::new(), &action.command))
    .await
    .map(|results| results.join("\n"))
}

#[cfg(test)]
mod tests {
  use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{
      atomic::{AtomicBool, AtomicUsize, Ordering},
      Mutex as StdMutex,
    },
  };

  use async_trait::async_trait;
  use tempfile::TempDir;
  use tokio::time::sleep;

  use super::*;
  use crate::task::RunMode;
  use octa_output::{ConsoleDiagnostic, ConsoleRecord};

  #[derive(Clone)]
  struct TestTask {
    id: String,
    internal: bool,
    requires_permit: bool,
    fails: bool,
    cancelled: bool,
    skipped: bool,
    failfast: bool,
    completed: Arc<AtomicBool>,
    running: Option<Arc<AtomicUsize>>,
    maximum_running: Option<Arc<AtomicUsize>>,
    output_scope: Option<ConsoleScope>,
  }

  struct RunningTaskGuard(Arc<AtomicUsize>);

  impl Drop for RunningTaskGuard {
    fn drop(&mut self) {
      self.0.fetch_sub(1, Ordering::SeqCst);
    }
  }

  impl PartialEq for TestTask {
    fn eq(&self, other: &Self) -> bool {
      self.id == other.id
    }
  }

  impl Eq for TestTask {}

  impl Hash for TestTask {
    fn hash<H: Hasher>(&self, state: &mut H) {
      self.id.hash(state);
    }
  }

  #[async_trait]
  impl Identifiable for TestTask {
    fn id(&self) -> String {
      self.id.clone()
    }

    fn name(&self) -> String {
      self.id.clone()
    }

    fn is_internal(&self) -> bool {
      self.internal
    }

    async fn get_deps_result(&self) -> HashMap<String, String> {
      HashMap::new()
    }
  }

  impl TaskItem for TestTask {
    fn run_mode(&self) -> RunMode {
      RunMode::Always
    }

    fn failfast(&self) -> bool {
      self.failfast
    }

    fn requires_concurrency_permit(&self) -> bool {
      self.requires_permit
    }

    fn output_scope(&self) -> Option<ConsoleScope> {
      self.output_scope.clone()
    }
  }

  #[async_trait]
  impl Executable<TestTask> for TestTask {
    async fn execute(&self, _runtime: TaskRuntime, cancel_token: CancellationToken) -> ExecutorResult<TaskOutcome> {
      let _running_guard = self.running.as_ref().map(|running| {
        let running_count = running.fetch_add(1, Ordering::SeqCst) + 1;
        self
          .maximum_running
          .as_ref()
          .unwrap()
          .fetch_max(running_count, Ordering::SeqCst);
        RunningTaskGuard(running.clone())
      });

      if self.fails {
        sleep(Duration::from_millis(20)).await;
        return Err(ExecutorError::TaskFailed(self.id.clone()));
      }

      if self.cancelled {
        return Err(ExecutorError::TaskCancelled(self.id.clone()));
      }

      if self.skipped {
        return Ok(TaskOutcome::skipped(self.id.clone()));
      }

      select! {
        _ = sleep(Duration::from_millis(150)) => {
          self.completed.store(true, Ordering::SeqCst);
          Ok(TaskOutcome::success(self.id.clone()))
        },
        _ = cancel_token.cancelled() => Err(ExecutorError::TaskCancelled(self.id.clone())),
      }
    }

    async fn set_result(&self, _task_name: String, _result: String) {}

    async fn bypass_result(&self, _result: HashMap<String, String>) {}
  }

  fn test_task(id: impl Into<String>) -> TestTask {
    TestTask {
      id: id.into(),
      internal: false,
      requires_permit: true,
      fails: false,
      cancelled: false,
      skipped: false,
      failfast: false,
      completed: Arc::new(AtomicBool::new(false)),
      running: None,
      maximum_running: None,
      output_scope: None,
    }
  }

  #[derive(Clone)]
  struct RecordingRenderer {
    events: Arc<StdMutex<Vec<ConsoleRecord>>>,
  }

  impl octa_output::ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &octa_output::ConsoleEntry) -> std::io::Result<()> {
      self.events.lock().unwrap().push(entry.record().clone());
      Ok(())
    }
  }

  fn test_executor(dag: DAG<TestTask>, config: ExecutorConfig) -> Executor<TestTask> {
    let plugin_dir = TempDir::new().unwrap();
    Executor::new(
      Arc::new(PluginManager::new(plugin_dir.path())),
      dag,
      config,
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap()
  }

  #[tokio::test]
  async fn emits_complete_scope_lifecycle_for_success_skip_and_failure() {
    for (fails, skipped, expected) in [
      (false, false, ConsoleStatus::Success),
      (false, true, ConsoleStatus::Skipped),
      (true, false, ConsoleStatus::Failed),
    ] {
      let allocator = octa_output::ConsoleScopeAllocator::default();
      let scope = allocator.scope("task");
      let mut task = test_task("task");
      task.fails = fails;
      task.skipped = skipped;
      task.output_scope = Some(scope.clone());
      let mut dag = DAG::new();
      dag.add_node(Arc::new(task));
      let events = Arc::new(StdMutex::new(Vec::new()));
      let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
      let plugin_dir = TempDir::new().unwrap();
      let summary = Arc::new(Summary::new());
      let plan = ExecutionPlan::new(dag, HashMap::new(), vec![scope.clone()]);
      let executor = Executor::new(
        Arc::new(PluginManager::new(plugin_dir.path())),
        plan,
        ExecutorConfig {
          emit_run_events: true,
          console,
          run_id: Some(7),
          ..ExecutorConfig::default()
        },
        None,
        Arc::new(sled::Config::new().temporary(true).open().unwrap()),
        false,
        false,
        Some(summary.clone()),
      )
      .unwrap();

      let result = executor.execute(CancellationToken::new(), "task").await;
      assert_eq!(result.is_err(), fails);
      {
        let events = events.lock().unwrap();
        assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
          run_id: 7,
          scope: scope.clone(),
        })));
        assert!(
          events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
            run_id: 7,
            scope,
            status: expected,
          }))
        );
        assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::RunFinished {
          run_id: 7,
          command: "task".to_owned(),
          status: expected,
        })));
      }
      assert_eq!(
        summary.report().await.tasks.len(),
        usize::from(expected == ConsoleStatus::Success)
      );
    }
  }

  struct RejectFirstScope(AtomicBool);

  impl octa_output::ConsoleRenderer for RejectFirstScope {
    fn render(&mut self, entry: &octa_output::ConsoleEntry) -> std::io::Result<()> {
      if matches!(
        entry.record(),
        ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { .. })
      ) && !self.0.swap(true, Ordering::SeqCst)
      {
        return Err(std::io::Error::other("scope start failed"));
      }
      Ok(())
    }
  }

  #[tokio::test]
  async fn renderer_failure_before_dag_start_does_not_run_deferred_actions() {
    let completed = Arc::new(AtomicBool::new(false));
    let deferred_task = {
      let mut task = test_task("cleanup");
      task.completed = completed.clone();
      Arc::new(task)
    };
    let mut deferred_dag = DAG::new();
    deferred_dag.add_node(deferred_task);
    let action = Arc::new(DeferredAction {
      command: "cleanup".to_owned(),
      plan: ExecutionPlan::new(deferred_dag, HashMap::new(), Vec::new()),
      order: 0,
      registered_after: Vec::new(),
    });

    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("main");
    let mut barrier = test_task("barrier");
    barrier.output_scope = Some(scope.clone());
    let mut dag = DAG::new();
    dag.add_node(Arc::new(barrier));
    let plan = ExecutionPlan::new(dag, HashMap::from([("barrier".to_owned(), action)]), vec![scope]);
    let plugin_dir = TempDir::new().unwrap();
    let executor = Executor::new(
      Arc::new(PluginManager::new(plugin_dir.path())),
      plan,
      ExecutorConfig {
        console: Arc::new(Console::new(RejectFirstScope(AtomicBool::new(false)))),
        emit_run_events: true,
        ..ExecutorConfig::default()
      },
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap();

    assert!(executor.execute(CancellationToken::new(), "main").await.is_err());
    assert!(!completed.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn deferred_cleanup_failures_are_reported_without_replacing_the_main_result() {
    let mut cleanup = test_task("cleanup");
    cleanup.fails = true;
    let mut cleanup_dag = DAG::new();
    cleanup_dag.add_node(Arc::new(cleanup));
    let action = Arc::new(DeferredAction {
      command: "cleanup".to_owned(),
      plan: ExecutionPlan::new(cleanup_dag, HashMap::new(), Vec::new()),
      order: 0,
      registered_after: Vec::new(),
    });
    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      ExecutionPlan::new(
        DAG::new(),
        HashMap::from([("cleanup-barrier".to_owned(), action)]),
        Vec::new(),
      ),
      ExecutorConfig {
        console,
        emit_run_events: true,
        ..ExecutorConfig::default()
      },
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap();

    executor.run_deferred(None).await;

    assert!(events.lock().unwrap().iter().any(|event| matches!(
      event,
      ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        level: ConsoleLevel::Error,
        message,
        ..
      }) if message.contains("Deferred command failed")
    )));
  }

  #[tokio::test]
  async fn cancellation_is_reflected_in_scope_and_run_events() {
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let mut task = test_task("build");
    task.cancelled = true;
    task.output_scope = Some(scope.clone());
    let mut dag = DAG::new();
    dag.add_node(Arc::new(task));
    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      ExecutionPlan::new(dag, HashMap::new(), vec![scope.clone()]),
      ExecutorConfig {
        console,
        emit_run_events: true,
        ..ExecutorConfig::default()
      },
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap();
    let result = executor.execute(CancellationToken::new(), "build").await;

    assert!(matches!(result, Err(ExecutorError::TaskCancelled(_))));
    let events = events.lock().unwrap();
    assert!(
      events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
        run_id: executor.run_id,
        scope,
        status: ConsoleStatus::Cancelled,
      }))
    );
    assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::RunFinished {
      run_id: executor.run_id,
      command: "build".to_owned(),
      status: ConsoleStatus::Cancelled,
    })));
  }

  async fn execute_parallel_failure(
    executor_failfast: bool,
    task_failfast: bool,
  ) -> (ExecutorResult<Vec<String>>, bool) {
    let completed = Arc::new(AtomicBool::new(false));
    let mut dag = DAG::new();
    let mut failure = test_task("failure");
    failure.fails = true;
    failure.failfast = task_failfast;
    dag.add_node(Arc::new(failure));
    let mut slow = test_task("slow");
    slow.completed = completed.clone();
    dag.add_node(Arc::new(slow));

    let executor = test_executor(
      dag,
      ExecutorConfig {
        failfast: executor_failfast,
        ..ExecutorConfig::default()
      },
    );

    let result = executor.execute(CancellationToken::new(), "test").await;
    (result, completed.load(Ordering::SeqCst))
  }

  #[tokio::test]
  async fn waits_for_running_tasks_by_default() {
    let (result, completed) = execute_parallel_failure(false, false).await;

    assert!(matches!(result, Err(ExecutorError::TaskFailed(_))));
    assert!(completed);
  }

  #[tokio::test]
  async fn failfast_cancels_running_tasks_from_config_or_task() {
    for (executor_failfast, task_failfast) in [(true, false), (false, true)] {
      let (result, completed) = execute_parallel_failure(executor_failfast, task_failfast).await;

      assert!(matches!(result, Err(ExecutorError::TaskFailed(_))));
      assert!(!completed);
    }
  }

  #[tokio::test]
  async fn limits_concurrently_running_tasks() {
    let running = Arc::new(AtomicUsize::new(0));
    let maximum_running = Arc::new(AtomicUsize::new(0));
    let concurrency = Arc::new(Semaphore::new(2));
    let build_executor = |prefix: &str| {
      let mut dag = DAG::new();
      for index in 0..2 {
        let mut task = test_task(format!("{prefix}-{index}"));
        task.running = Some(running.clone());
        task.maximum_running = Some(maximum_running.clone());
        dag.add_node(Arc::new(task));
      }

      test_executor(
        dag,
        ExecutorConfig {
          concurrency: Some(concurrency.clone()),
          ..ExecutorConfig::default()
        },
      )
    };
    let first = build_executor("first");
    let second = build_executor("second");

    let (first_result, second_result) = tokio::join!(
      first.execute(CancellationToken::new(), "first"),
      second.execute(CancellationToken::new(), "second"),
    );

    assert!(first_result.is_ok());
    assert!(second_result.is_ok());
    assert_eq!(maximum_running.load(Ordering::SeqCst), 2);
  }

  #[tokio::test]
  async fn returns_error_when_concurrency_limiter_is_closed() {
    let mut dag = DAG::new();
    dag.add_node(Arc::new(test_task("task")));
    let concurrency = Arc::new(Semaphore::new(1));
    concurrency.close();
    let executor = test_executor(
      dag,
      ExecutorConfig {
        concurrency: Some(concurrency),
        ..ExecutorConfig::default()
      },
    );

    let result = executor.execute(CancellationToken::new(), "test").await;

    assert!(matches!(result, Err(ExecutorError::ConcurrencyLimiterClosed)));
  }

  #[tokio::test]
  async fn cancellation_interrupts_waiting_for_concurrency_permit() {
    let completed = Arc::new(AtomicBool::new(false));
    let mut dag = DAG::new();
    let mut task = test_task("task");
    task.completed = completed.clone();
    dag.add_node(Arc::new(task));
    let concurrency = Arc::new(Semaphore::new(1));
    let _permit = concurrency.clone().acquire_owned().await.unwrap();
    let executor = test_executor(
      dag,
      ExecutorConfig {
        concurrency: Some(concurrency),
        ..ExecutorConfig::default()
      },
    );
    let cancel_token = CancellationToken::new();
    let execution_token = cancel_token.clone();
    let execution = tokio::spawn(async move { executor.execute(execution_token, "test").await });
    tokio::task::yield_now().await;
    cancel_token.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), execution)
      .await
      .unwrap()
      .unwrap();

    assert!(result.is_ok());
    assert!(!completed.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn internal_tasks_do_not_acquire_concurrency_permits() {
    let completed = Arc::new(AtomicBool::new(false));
    let mut dag = DAG::new();
    let mut task = test_task("internal");
    task.internal = true;
    task.requires_permit = false;
    task.completed = completed.clone();
    dag.add_node(Arc::new(task));
    let executor = test_executor(
      dag,
      ExecutorConfig {
        concurrency: Some(Arc::new(Semaphore::new(0))),
        ..ExecutorConfig::default()
      },
    );

    let result = tokio::time::timeout(
      Duration::from_secs(1),
      executor.execute(CancellationToken::new(), "test"),
    )
    .await
    .unwrap();

    assert!(result.is_ok());
    assert!(completed.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn hidden_executable_tasks_acquire_concurrency_permits() {
    let mut dag = DAG::new();
    let mut task = test_task("condition");
    task.internal = true;
    dag.add_node(Arc::new(task));
    let concurrency = Arc::new(Semaphore::new(1));
    concurrency.close();
    let executor = test_executor(
      dag,
      ExecutorConfig {
        concurrency: Some(concurrency),
        ..ExecutorConfig::default()
      },
    );

    let result = executor.execute(CancellationToken::new(), "test").await;

    assert!(matches!(result, Err(ExecutorError::ConcurrencyLimiterClosed)));
  }

  #[test]
  fn originating_error_replaces_secondary_cancellation() {
    let mut recorded = Some(ExecutorError::TaskCancelled("sibling".to_string()));

    record_execution_error(&mut recorded, ExecutorError::TaskFailed("origin".to_string()));
    record_execution_error(&mut recorded, ExecutorError::TaskCancelled("later".to_string()));

    assert!(matches!(recorded, Some(ExecutorError::TaskFailed(task)) if task == "origin"));
  }
}
