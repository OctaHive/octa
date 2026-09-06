//! Concurrent DAG scheduling and execution-result assembly.
//!
//! This module consumes an already expanded execution plan. It coordinates
//! dependency readiness, concurrency permits, fail-fast cancellation, deferred
//! actions, and lifecycle completion without knowing how Octafile declarations
//! were parsed or how events are presented.

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

use chrono::Utc;
use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
#[cfg(test)]
use indexmap::IndexMap;
use octa_dag::{Identifiable, DAG};
#[cfg(test)]
use octa_output::{Console, ExecutionEvent};
use octa_output::{ConsoleLevel, ConsoleScope, ConsoleStatus};
#[cfg(test)]
use octa_plugin_manager::plugin_manager::PluginManager;
use tokio::{
  select,
  sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
  task::JoinHandle,
  time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info_span, Instrument};

#[cfg(test)]
use crate::task::TaskItem;
#[cfg(test)]
use crate::terminal::UnsupportedRawTerminal;
use crate::{
  error::{ExecutorError, ExecutorResult},
  execution_handle::ExecutionHandle,
  execution_recorder::ExecutionRecorder,
  execution_result::{conclusion, ExecutionFailure, ExecutionResult, TaskResult, TaskRole},
  execution_run::ExecutionRun,
  interactive_scope_tracker::InteractiveScopeTracker,
  runtime_coordinator::RuntimeCoordinator,
  summary::{Summary, TaskSummaryItem},
  task::{Executable, ExecutionBinding, TaskOutcome, TaskRuntime},
};

// Long enough for cooperative plugin cancellation, but finite so a broken
// plugin cannot keep an agent process alive forever.
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

/// Ensures numeric task and step IDs are unique within the plan's allocator domain.
fn validate_execution_identities(scopes: &[ConsoleScope], bindings: &[ExecutionBinding]) -> ExecutorResult<()> {
  let mut identities = scopes.iter().chain(bindings.iter().map(ExecutionBinding::scope));
  let Some(first) = identities.next() else {
    return Ok(());
  };
  if identities.all(|scope| first.shares_allocator_with(scope)) {
    Ok(())
  } else {
    Err(ExecutorError::ExecutionIdentityError(
      "all task scopes in a plan must use one ConsoleScopeAllocator".to_owned(),
    ))
  }
}

/// Intermediate scheduler result before run-level lifecycle is finalized.
struct PlanExecution {
  outputs: Vec<String>,
  nested_tasks: Vec<TaskResult>,
  failure: Option<ExecutorError>,
  cancelled: bool,
}

/// Value returned by one spawned DAG node.
struct NodeExecution {
  output: Arc<str>,
  nested_tasks: Vec<TaskResult>,
}

/// Marks results from nested cleanup plans without changing their conclusions.
fn mark_deferred(tasks: &mut [TaskResult]) {
  for task in tasks {
    task.role = TaskRole::Deferred;
  }
}

/// Forces unfinished Tokio tasks down and observes every join result.
async fn abort_and_join(handles: Vec<JoinHandle<ExecutorResult<NodeExecution>>>) {
  for handle in &handles {
    handle.abort();
  }
  let _ = join_all(handles).await;
}

/// A task graph together with cleanup actions managed by the executor.
///
/// Deferred actions are intentionally kept outside `TaskNode`: task nodes describe work,
/// while the execution plan describes when additional work must run.
#[derive(Clone)]
pub(crate) struct ExecutionPlan<T: Eq + Hash + Identifiable> {
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
  pub(crate) fn is_linear(&self) -> ExecutorResult<bool> {
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

/// Scheduling services that are not part of per-node execution.
#[derive(Clone)]
pub(crate) struct ExecutorConfig {
  /// Cancel tasks that are already running when any task in the plan fails.
  pub(crate) failfast: bool,
  /// Shared limiter for executable work; graph barriers do not consume permits.
  pub(crate) concurrency: Option<Arc<Semaphore>>,
  /// Lifecycle shared with the planning phase when execution is started through the high-level API.
  pub(crate) run: Option<Arc<ExecutionRun>>,
  /// Isolation shared by independently built plans in one execution batch.
  pub(crate) runtime_coordinator: Arc<RuntimeCoordinator>,
  /// Optional application-owned task timing aggregate.
  pub(crate) summary: Option<Arc<Summary>>,
}

impl Default for ExecutorConfig {
  fn default() -> Self {
    Self {
      failfast: false,
      concurrency: None,
      run: None,
      runtime_coordinator: Arc::new(RuntimeCoordinator::default()),
      summary: None,
    }
  }
}

/// Immutable services and shared scheduler state used by all nodes in one plan.
struct ExecutorContext<T: Hash + Identifiable + Eq> {
  /// Immutable dependency graph shared by all spawned nodes.
  dag: Arc<DAG<T>>,
  /// Deferred plans indexed by their main-graph barrier node.
  deferred: Arc<HashMap<String, Arc<DeferredAction<T>>>>,
  /// Stops queue consumption once every reachable node has completed.
  finished: CancellationToken,
  /// Remaining dependency count for each node.
  in_degree: Arc<Mutex<HashMap<String, usize>>>,
  /// Scheduled nodes that have not yet published completion.
  active_tasks: Arc<AtomicUsize>,
  /// Successful task timings for optional presentation after the run.
  summary: Arc<Summary>,
  // Successful normal nodes determine which deferred actions were registered before interruption;
  // deferred barriers are also recorded when attempted so cleanup is never executed twice.
  completed_tasks: Arc<Mutex<HashSet<String>>>,
  failfast: bool,
  concurrency: Option<Arc<Semaphore>>,
  recorder: Arc<ExecutionRecorder>,
  interactive_tracker: Arc<InteractiveScopeTracker>,
  runtime_coordinator: Arc<RuntimeCoordinator>,
  runtime: TaskRuntime,
}

/// Executor manages the execution of tasks in a directed acyclic graph (DAG)
pub(crate) struct Executor<T: Eq + Hash + Executable + Send + Sync + Clone + 'static> {
  context: Arc<ExecutorContext<T>>,
  run: Option<Arc<ExecutionRun>>,
}

impl<T: Eq + Hash + Executable + Send + Sync + Clone + 'static> Executor<T> {
  /// Creates a new Executor instance with the given DAG
  pub(crate) fn new(
    plan: impl Into<ExecutionPlan<T>>,
    config: ExecutorConfig,
    runtime: TaskRuntime,
  ) -> ExecutorResult<Self> {
    let plan = plan.into();
    let dag = plan.dag;
    let node_bindings = dag
      .nodes()
      .iter()
      .filter_map(|node| node.execution_binding())
      .collect::<Vec<_>>();
    validate_execution_identities(&plan.scopes, &node_bindings)?;
    let run_id = runtime.run_id;
    let run = config.run.clone();
    let recorder = Arc::new(ExecutionRecorder::new(
      runtime.console.clone(),
      run_id,
      plan.scopes,
      node_bindings,
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
    let in_degree = dag.nodes().iter().map(|n| (n.id().to_owned(), 0)).collect();

    let finished = CancellationToken::new();
    let context = ExecutorContext {
      dag: Arc::new(dag),
      deferred: Arc::new(plan.deferred),
      finished,
      in_degree: Arc::new(Mutex::new(in_degree)),
      active_tasks: Arc::new(AtomicUsize::new(0)),
      summary: config.summary.unwrap_or_else(|| Arc::new(Summary::new())),
      completed_tasks: Arc::new(Mutex::new(HashSet::new())),
      failfast: config.failfast,
      concurrency: config.concurrency,
      recorder,
      interactive_tracker,
      runtime_coordinator: config.runtime_coordinator,
      runtime,
    };

    Ok(Self {
      context: Arc::new(context),
      run,
    })
  }

  /// Executes all tasks in the DAG and returns their complete terminal state.
  ///
  /// Task, command, plugin, timeout, and cancellation failures are represented by
  /// [`ExecutionResult::conclusion`]. An [`ExecutorError`] is returned only when a complete
  /// result cannot be formed or its terminal lifecycle cannot be published.
  pub(crate) async fn execute(
    &self,
    cancel_token: CancellationToken,
    command: &str,
  ) -> ExecutorResult<ExecutionResult> {
    let started_at = match self.begin_execution(command).await {
      Ok(started_at) => started_at,
      Err(error) => {
        self.finish_failed_start(command).await;
        return Err(error);
      },
    };
    let plan = match self.execute_plan(cancel_token).await {
      Ok(plan) => plan,
      Err(error) => PlanExecution {
        outputs: Vec::new(),
        nested_tasks: Vec::new(),
        failure: Some(error),
        cancelled: false,
      },
    };
    let deferred_exit_code = plan.failure.as_ref().and_then(ExecutorError::command_exit_code);
    self.context.interactive_tracker.finish_remaining().await;
    let deferred_result = self.run_deferred(deferred_exit_code).await;

    let mut status = if deferred_result.is_err() {
      ConsoleStatus::Failed
    } else {
      match &plan.failure {
        Some(ExecutorError::TaskCancelled(_)) => ConsoleStatus::Cancelled,
        Some(_) => ConsoleStatus::Failed,
        None if plan.cancelled => ConsoleStatus::Cancelled,
        None => self.context.recorder.successful_run_status().await,
      }
    };
    let finish_result = self.context.recorder.finish_remaining(status).await;
    if finish_result.is_err() {
      status = ConsoleStatus::Failed;
    }
    finish_result?;
    let finished_at = match &self.run {
      Some(run) => run.finish(command, status).await?,
      None => Utc::now(),
    };
    let deferred_tasks = deferred_result?;

    let mut tasks = self.context.recorder.results().await?;
    tasks.extend(plan.nested_tasks);
    tasks.extend(deferred_tasks);
    tasks.sort_by_key(|task| task.task_id);
    let failure = self.context.recorder.failure().await.or_else(|| {
      plan
        .failure
        .as_ref()
        .map(|error| ExecutionFailure::from_error(error, None))
    });
    Ok(ExecutionResult {
      run_id: self.context.runtime.run_id,
      command: command.to_owned(),
      started_at,
      finished_at,
      conclusion: conclusion(status, failure, None, None),
      tasks,
      outputs: plan.outputs,
    })
  }

  /// Starts this already-built plan on the current Tokio runtime.
  pub(crate) fn start(self, command: impl Into<String>) -> ExecutionHandle {
    self.spawn_execution(CancellationToken::new(), command.into())
  }

  /// Starts this already-built plan below an application-owned cancellation token.
  pub(crate) fn start_with_token(
    self,
    parent_cancellation: &CancellationToken,
    command: impl Into<String>,
  ) -> ExecutionHandle {
    self.spawn_execution(parent_cancellation.child_token(), command.into())
  }

  fn spawn_execution(self, cancellation: CancellationToken, command: String) -> ExecutionHandle {
    let run_id = self.context.runtime.run_id;
    let execution_command = command.clone();
    let execution_cancellation = cancellation.clone();
    let task = tokio::spawn(async move { self.execute(execution_cancellation, &execution_command).await });
    ExecutionHandle::new(run_id, command, cancellation, task)
  }

  /// Publishes run and scope declarations before a later call to [`Self::execute`].
  ///
  /// Batch schedulers can prepare executors in declaration order and then execute them
  /// concurrently. Calling this more than once with the same command is idempotent. A prepared
  /// executor is single-use and must subsequently be passed to [`Self::execute`].
  pub(crate) async fn prepare(&self, command: &str) -> ExecutorResult<()> {
    if let Err(error) = self.begin_execution(command).await {
      self.finish_failed_start(command).await;
      return Err(error);
    }
    Ok(())
  }

  async fn begin_execution(&self, command: &str) -> ExecutorResult<chrono::DateTime<Utc>> {
    let started_at = match &self.run {
      Some(run) => run.start(command).await?,
      None => Utc::now(),
    };
    self.context.recorder.declare().await?;
    Ok(started_at)
  }

  async fn finish_failed_start(&self, command: &str) {
    let _ = self.context.recorder.finish_remaining(ConsoleStatus::Failed).await;
    if let Some(run) = &self.run {
      let _ = run.finish(command, ConsoleStatus::Failed).await;
    }
  }

  async fn execute_plan(&self, cancel_token: CancellationToken) -> ExecutorResult<PlanExecution> {
    self.initialize_in_degrees().await?;
    let (tx, rx) = self.create_task_channel();
    let mut handles = Vec::with_capacity(self.context.dag.node_count());
    // Internal cancellation must not cancel the caller's token: the caller may reuse it for
    // another top-level task or a subsequent watch iteration.
    let execution_token = cancel_token.child_token();

    self.schedule_initial_tasks(&tx).await?;
    self.process_tasks(execution_token, rx, &tx, &mut handles).await;
    self.handle_completion(cancel_token, handles).await
  }

  fn create_task_channel(&self) -> (mpsc::Sender<Arc<T>>, mpsc::Receiver<Arc<T>>) {
    mpsc::channel(self.context.dag.node_count().max(1))
  }

  async fn run_deferred(&self, exit_code: Option<i32>) -> ExecutorResult<Vec<TaskResult>> {
    let completed_tasks = self.context.completed_tasks.lock().await.clone();
    let mut tasks = Vec::new();
    let mut first_error = None;
    let mut deferred = self
      .context
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

      let result = execute_deferred_action(
        action,
        TaskRuntime {
          deferred_exit_code: exit_code,
          ..self.context.runtime.clone()
        },
        self.context.concurrency.clone(),
        self.context.runtime_coordinator.clone(),
      )
      .await;
      match result {
        Ok(mut result) => {
          let failure = result.failure().map(ToString::to_string);
          mark_deferred(&mut result.tasks);
          tasks.extend(result.tasks);
          if let Some(failure) = failure {
            if let Err(error) = self
              .context
              .runtime
              .console
              .run_message(
                self.context.runtime.run_id,
                ConsoleLevel::Error,
                format!("Deferred command failed: {failure}"),
              )
              .await
            {
              record_execution_error(&mut first_error, error.into());
            }
          }
        },
        Err(error) => {
          let message = ExecutionFailure::from_error(&error, None).to_string();
          if let Err(log_error) = self
            .context
            .runtime
            .console
            .run_message(
              self.context.runtime.run_id,
              ConsoleLevel::Error,
              format!("Deferred command failed: {message}"),
            )
            .await
          {
            record_execution_error(&mut first_error, log_error.into());
          }
          record_execution_error(&mut first_error, error);
        },
      }
    }
    first_error.map_or(Ok(tasks), Err)
  }

  async fn handle_completion(
    &self,
    cancel_token: CancellationToken,
    handles: Vec<JoinHandle<ExecutorResult<NodeExecution>>>,
  ) -> ExecutorResult<PlanExecution> {
    if cancel_token.is_cancelled() {
      self.shutdown(handles).await
    } else {
      self.complete_execution(handles).await
    }
  }

  /// Drains ready nodes until cancellation or graph completion closes the queue.
  async fn process_tasks(
    &self,
    cancel_token: CancellationToken,
    mut rx: mpsc::Receiver<Arc<T>>,
    tx: &mpsc::Sender<Arc<T>>,
    handles: &mut Vec<JoinHandle<ExecutorResult<NodeExecution>>>,
  ) {
    while let Some(task) = self.receive_next_task(&mut rx, &cancel_token).await {
      handles.push(self.spawn_task(cancel_token.clone(), task, tx.clone()));
    }
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
        _ = self.context.finished.cancelled() => None
    }
  }

  /// Spawns one node with shared executor state and execution-local cancellation.
  fn spawn_task(
    &self,
    cancel_token: CancellationToken,
    task: Arc<T>,
    tx: mpsc::Sender<Arc<T>>,
  ) -> JoinHandle<ExecutorResult<NodeExecution>> {
    let context = self.context.clone();
    let run_id = context.runtime.run_id;
    // Runtime tracing diagnostics inherit the same correlation id carried by
    // structured task output without coupling the executor to a tracing layer.
    tokio::spawn(
      async move { TaskExecutor::new(context, task, tx, cancel_token).execute().await }
        .instrument(info_span!("task_execution", run_id)),
    )
  }

  /// Enqueues graph roots; deferred barriers are released only by predecessors.
  async fn schedule_initial_tasks(&self, tx: &mpsc::Sender<Arc<T>>) -> ExecutorResult<()> {
    let initial = {
      let degrees = self.context.in_degree.lock().await;
      self
        .context
        .dag
        .nodes()
        .iter()
        // Deferred barriers are released only by their predecessors, never as graph roots.
        .filter(|node| !self.context.deferred.contains_key(node.id()) && degrees[node.id()] == 0)
        .cloned()
        .collect::<Vec<_>>()
    };
    for node in &initial {
      self.context.active_tasks.fetch_add(1, Ordering::SeqCst);
      tx.send(node.clone()).await.map_err(|_| ExecutorError::ChannelError)?;
    }
    if initial.is_empty() {
      self.context.finished.cancel();
    }
    Ok(())
  }

  /// Computes mutable in-degrees from the immutable graph before scheduling.
  async fn initialize_in_degrees(&self) -> ExecutorResult<()> {
    let mut degrees = self.context.in_degree.lock().await;
    for deps in self.context.dag.edges().values() {
      for node in deps {
        *degrees
          .get_mut(node.id())
          .ok_or_else(|| ExecutorError::TaskNotFound(node.id().to_owned()))? += 1;
      }
    }

    Ok(())
  }

  async fn complete_execution(
    &self,
    handles: Vec<JoinHandle<ExecutorResult<NodeExecution>>>,
  ) -> ExecutorResult<PlanExecution> {
    let mut indexed_outputs = Vec::new();
    let mut nested_tasks = Vec::new();
    let mut first_error = None;
    let mut handles = handles
      .into_iter()
      .enumerate()
      .map(|(index, handle)| async move { (index, handle.await) })
      .collect::<FuturesUnordered<_>>();

    while let Some((index, result)) = handles.next().await {
      match result {
        Ok(Ok(result)) => {
          indexed_outputs.push((index, result.output));
          nested_tasks.extend(result.nested_tasks);
        },
        Ok(Err(error)) => record_execution_error(&mut first_error, error),
        Err(error) => record_execution_error(&mut first_error, ExecutorError::JoinError(error)),
      }
    }
    indexed_outputs.sort_by_key(|(index, _)| *index);

    Ok(PlanExecution {
      outputs: indexed_outputs
        .into_iter()
        .map(|(_, output)| output.to_string())
        .collect(),
      nested_tasks,
      failure: first_error,
      cancelled: false,
    })
  }

  async fn shutdown(&self, handles: Vec<JoinHandle<ExecutorResult<NodeExecution>>>) -> ExecutorResult<PlanExecution> {
    self.shutdown_with_timeout(handles, SHUTDOWN_TIMEOUT).await
  }

  async fn shutdown_with_timeout(
    &self,
    mut handles: Vec<JoinHandle<ExecutorResult<NodeExecution>>>,
    shutdown_timeout: Duration,
  ) -> ExecutorResult<PlanExecution> {
    if let Err(error) = self.log_info("Initiating graceful shutdown").await {
      abort_and_join(handles).await;
      return Err(error);
    }

    match timeout(shutdown_timeout, join_all(handles.iter_mut())).await {
      Ok(results) => self.handle_shutdown_results(results).await,
      Err(_) => {
        abort_and_join(handles).await;
        let _ = self.log_error("Shutdown timeout exceeded, forcing shutdown").await;
        Ok(PlanExecution {
          outputs: Vec::new(),
          nested_tasks: Vec::new(),
          failure: Some(ExecutorError::ShutdownTimeout),
          cancelled: true,
        })
      },
    }
  }

  async fn handle_shutdown_results(
    &self,
    results: Vec<Result<ExecutorResult<NodeExecution>, tokio::task::JoinError>>,
  ) -> ExecutorResult<PlanExecution> {
    let mut outputs = Vec::new();
    let mut nested_tasks = Vec::new();
    let mut first_error = None;
    for result in results {
      match result {
        Ok(Ok(result)) => {
          outputs.push(result.output.to_string());
          nested_tasks.extend(result.nested_tasks);
        },
        Ok(Err(error)) => {
          let message = ExecutionFailure::from_error(&error, None).to_string();
          self
            .log_error(&format!("Task failed during shutdown: {message}"))
            .await?;
          record_execution_error(&mut first_error, error);
        },
        Err(error) => record_execution_error(&mut first_error, ExecutorError::JoinError(error)),
      }
    }
    self.log_info("Graceful shutdown completed").await?;
    Ok(PlanExecution {
      outputs,
      nested_tasks,
      failure: first_error,
      cancelled: true,
    })
  }

  async fn log_info(&self, message: &str) -> ExecutorResult<()> {
    self
      .context
      .runtime
      .console
      .run_message(self.context.runtime.run_id, ConsoleLevel::Info, message)
      .await?;
    Ok(())
  }

  async fn log_error(&self, message: &str) -> ExecutorResult<()> {
    self
      .context
      .runtime
      .console
      .run_message(self.context.runtime.run_id, ConsoleLevel::Error, message)
      .await?;
    Ok(())
  }
}

struct TaskExecutor<T: Executable + Hash + Eq + Send + Sync + Clone + 'static> {
  context: Arc<ExecutorContext<T>>,
  task: Arc<T>,
  tx: mpsc::Sender<Arc<T>>,
  cancel_token: CancellationToken,
}

impl<T: Executable + Hash + Eq + Send + Sync + Clone + 'static> TaskExecutor<T> {
  fn new(
    context: Arc<ExecutorContext<T>>,
    task: Arc<T>,
    tx: mpsc::Sender<Arc<T>>,
    cancel_token: CancellationToken,
  ) -> Self {
    Self {
      context,
      task,
      tx,
      cancel_token,
    }
  }

  async fn execute(self) -> ExecutorResult<NodeExecution> {
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

  async fn execute_locked(&self) -> ExecutorResult<NodeExecution> {
    let task_name = self.task.id();
    debug!("Executing task: {}", task_name);

    let binding = self.task.execution_binding();
    if let Some(binding) = &binding {
      if let Err(error) = self.context.recorder.start_scope(binding).await {
        let result = self.handle_error(error.into(), Some(binding)).await;
        return self
          .complete_scope(result, Some(binding))
          .await
          .map(|output| NodeExecution {
            output,
            nested_tasks: Vec::new(),
          });
      }
    }

    // Barriers do not consume capacity. Hidden executable nodes such as conditions do.
    let _permit = match self.acquire_permit().await {
      Ok(permit) => permit,
      Err(error) => {
        let result = self.handle_error(error, binding.as_ref()).await;
        return self
          .complete_scope(result, binding.as_ref())
          .await
          .map(|output| NodeExecution {
            output,
            nested_tasks: Vec::new(),
          });
      },
    };
    // A step starts after scheduler capacity has been acquired. Conditions,
    // freshness and cache checks are part of evaluating that executable step;
    // a cancellation while queued therefore finishes it without a start event.
    if let Some(binding) = &binding {
      if let Err(error) = self.context.recorder.start_step(binding).await {
        let result = self.handle_error(error.into(), Some(binding)).await;
        return self
          .complete_scope(result, Some(binding))
          .await
          .map(|output| NodeExecution {
            output,
            nested_tasks: Vec::new(),
          });
      }
    }
    let start_time = SystemTime::now();
    let deferred = self.context.deferred.get(task_name).cloned();
    let mut nested_tasks = Vec::new();
    let result = if let Some(action) = deferred.as_ref() {
      // Reaching the barrier consumes this cleanup action even when its nested lifecycle fails;
      // shutdown cleanup must never execute the same action a second time.
      self.context.completed_tasks.lock().await.insert(task_name.to_owned());
      // The graph node is only an ordering barrier; its actual work lives in the nested plan.
      match execute_deferred_action(
        action.clone(),
        self.context.runtime.clone(),
        self.context.concurrency.clone(),
        self.context.runtime_coordinator.clone(),
      )
      .await
      {
        Ok(mut result) => {
          let failure = result.failure().map(ToString::to_string);
          let output = result.outputs.join("\n");
          mark_deferred(&mut result.tasks);
          nested_tasks = result.tasks;
          if let Some(failure) = failure {
            match self
              .context
              .runtime
              .console
              .run_message(
                self.context.runtime.run_id,
                ConsoleLevel::Error,
                format!("Deferred command failed: {failure}"),
              )
              .await
            {
              Ok(()) => Ok(TaskOutcome::success(output)),
              Err(error) => Err(error.into()),
            }
          } else {
            Ok(TaskOutcome::success(output))
          }
        },
        Err(error) => Err(error),
      }
    } else {
      self
        .task
        .execute(self.context.runtime.clone(), self.cancel_token.clone())
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
      Err(e) => self.handle_error(e, binding.as_ref()).await,
    };
    self
      .complete_scope(result, binding.as_ref())
      .await
      .map(|output| NodeExecution { output, nested_tasks })
  }

  async fn complete_scope(
    &self,
    result: ExecutorResult<TaskOutcome>,
    binding: Option<&ExecutionBinding>,
  ) -> ExecutorResult<Arc<str>> {
    let Some(binding) = binding else {
      return result.map(TaskOutcome::into_output);
    };
    let status = match &result {
      Ok(outcome) => outcome.status(),
      Err(ExecutorError::TaskCancelled(_)) => ConsoleStatus::Cancelled,
      Err(_) => ConsoleStatus::Failed,
    };
    let failure = result
      .as_ref()
      .err()
      .map(|error| ExecutionFailure::from_error(error, Some(binding)));
    let completion = self.context.recorder.complete(binding, status, failure).await;
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
      _ = self.cancel_token.cancelled() => Err(ExecutorError::TaskCancelled(self.task.name().to_owned())),
    }
  }

  async fn handle_success(
    &self,
    output: Arc<str>,
    status: ConsoleStatus,
    start_time: SystemTime,
  ) -> ExecutorResult<Arc<str>> {
    if self.cancel_token.is_cancelled() {
      debug!("Task {} cancelled during execution", self.task.id());
      return Ok(Arc::from(""));
    }

    if status == ConsoleStatus::Success && !self.task.is_internal() {
      if let Ok(elapsed) = start_time.elapsed() {
        self
          .context
          .summary
          .add(TaskSummaryItem {
            name: self.task.name().to_owned(),
            duration: elapsed,
          })
          .await;
      }
    }

    // Regular successful nodes register later deferred actions. Deferred barriers may already
    // have been recorded before their nested plan ran, so insertion is intentionally idempotent.
    self
      .context
      .completed_tasks
      .lock()
      .await
      .insert(self.task.id().to_owned());

    self.process_task_success(output).await
  }

  async fn handle_error(
    &self,
    error: ExecutorError,
    binding: Option<&ExecutionBinding>,
  ) -> ExecutorResult<TaskOutcome> {
    let failure = ExecutionFailure::from_error(&error, binding);
    let message = format!("Task {} failed: {}", self.task.name(), failure.message);
    let _ = match binding {
      Some(binding) => {
        self
          .context
          .runtime
          .console
          .run_message_at(
            self.context.runtime.run_id,
            binding.scope().clone(),
            ConsoleLevel::Error,
            message,
          )
          .await
      },
      None => {
        self
          .context
          .runtime
          .console
          .run_message(self.context.runtime.run_id, ConsoleLevel::Error, message)
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

  async fn process_task_success(&self, output: Arc<str>) -> ExecutorResult<Arc<str>> {
    if let Some(deps) = self.context.dag.edges().get(self.task.id()) {
      for dep in deps {
        if self.task.is_internal() {
          let res = self.task.get_deps_result().await;
          dep.bypass_result(res).await;
        } else {
          dep.set_result(self.task.name().to_owned(), output.clone()).await;
        }
      }

      let ready = {
        let mut degrees = self.context.in_degree.lock().await;
        let mut ready = Vec::new();
        for dep in deps {
          let dep_count = degrees
            .get_mut(dep.id())
            .ok_or_else(|| ExecutorError::TaskNotFound(dep.id().to_owned()))?;
          *dep_count -= 1;
          if *dep_count == 0 {
            ready.push(dep.clone());
          }
        }
        ready
      };
      for dep in ready {
        if !self.context.finished.is_cancelled() {
          self.context.active_tasks.fetch_add(1, Ordering::SeqCst);
          self.tx.send(dep).await.map_err(|_| ExecutorError::ChannelError)?;
        }
      }
    }

    if self.context.active_tasks.fetch_sub(1, Ordering::SeqCst) == 1 {
      self.context.finished.cancel();
    }

    Ok(output)
  }
}

async fn execute_deferred_action<T: Eq + Hash + Executable + Send + Sync + Clone + 'static>(
  action: Arc<DeferredAction<T>>,
  runtime: TaskRuntime,
  concurrency: Option<Arc<Semaphore>>,
  runtime_coordinator: Arc<RuntimeCoordinator>,
) -> ExecutorResult<ExecutionResult> {
  // A fresh token lets cleanup continue even when cancellation stopped the main plan.
  let executor = Executor::new(
    action.plan.clone(),
    ExecutorConfig {
      failfast: false,
      concurrency,
      run: None,
      runtime_coordinator,
      summary: None,
    },
    runtime,
  )?;

  Box::pin(executor.execute(CancellationToken::new(), &action.command)).await
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
  use crate::ExecutionConclusion;
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
    execution_binding: Option<ExecutionBinding>,
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

  impl Identifiable for TestTask {
    fn id(&self) -> &str {
      &self.id
    }
  }

  #[async_trait]
  impl TaskItem for TestTask {
    fn name(&self) -> &str {
      &self.id
    }

    fn is_internal(&self) -> bool {
      self.internal
    }

    async fn get_deps_result(&self) -> HashMap<String, Arc<str>> {
      HashMap::new()
    }

    fn failfast(&self) -> bool {
      self.failfast
    }

    fn requires_concurrency_permit(&self) -> bool {
      self.requires_permit
    }

    fn execution_binding(&self) -> Option<ExecutionBinding> {
      self.execution_binding.clone()
    }
  }

  #[async_trait]
  impl Executable for TestTask {
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

    async fn set_result(&self, _task_name: String, _result: Arc<str>) {}

    async fn bypass_result(&self, _result: HashMap<String, Arc<str>>) {}
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
      execution_binding: None,
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

  fn test_runtime(console: Arc<Console>, run_id: u64) -> TaskRuntime {
    let plugin_dir = TempDir::new().unwrap();
    TaskRuntime {
      plugin_manager: Arc::new(PluginManager::new(plugin_dir.path())),
      terminal: Arc::new(UnsupportedRawTerminal),
      cache: Arc::new(Mutex::new(IndexMap::new())),
      fingerprint: Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      console,
      run_id,
      dry: false,
      force: false,
      deferred_exit_code: None,
    }
  }

  fn test_executor(dag: DAG<TestTask>, config: ExecutorConfig) -> Executor<TestTask> {
    Executor::new(dag, config, test_runtime(Arc::new(Console::default()), 1)).unwrap()
  }

  fn test_executor_with_console(
    plan: ExecutionPlan<TestTask>,
    config: ExecutorConfig,
    console: Arc<Console>,
    run_id: u64,
  ) -> Executor<TestTask> {
    Executor::new(plan, config, test_runtime(console, run_id)).unwrap()
  }

  fn test_executor_with_run(
    plan: ExecutionPlan<TestTask>,
    mut config: ExecutorConfig,
    console: Arc<Console>,
    run_id: u64,
  ) -> Executor<TestTask> {
    config.run = Some(Arc::new(ExecutionRun::new(console.clone(), run_id)));
    test_executor_with_console(plan, config, console, run_id)
  }

  #[tokio::test]
  async fn execution_handle_exposes_identity_and_terminal_result() {
    let mut dag = DAG::new();
    dag.add_node(Arc::new(test_task("task")));
    let executor = test_executor(dag, ExecutorConfig::default());
    let expected_run_id = executor.context.runtime.run_id;

    let handle = executor.start("build");

    assert_eq!(handle.run_id(), expected_run_id);
    assert_eq!(handle.command(), "build");
    assert!(!handle.is_finished());
    assert!(!handle.cancellation_token().is_cancelled());
    let result = handle.wait().await.unwrap();
    assert_eq!(result.run_id, expected_run_id);
    assert_eq!(result.command, "build");
    assert!(result.is_success());
  }

  #[tokio::test]
  async fn execution_handle_cancels_and_waits_for_a_terminal_result() {
    let mut dag = DAG::new();
    dag.add_node(Arc::new(test_task("task")));
    let parent_cancellation = CancellationToken::new();
    let handle = test_executor(dag, ExecutorConfig::default()).start_with_token(&parent_cancellation, "build");
    let execution_cancellation = handle.cancellation_token();

    let result = handle.cancel_and_wait().await.unwrap();

    assert!(execution_cancellation.is_cancelled());
    assert!(!parent_cancellation.is_cancelled());
    assert!(matches!(result.conclusion, ExecutionConclusion::Cancelled(_)));
  }

  #[tokio::test]
  async fn execution_handle_inherits_parent_cancellation() {
    let mut dag = DAG::new();
    dag.add_node(Arc::new(test_task("task")));
    let parent_cancellation = CancellationToken::new();
    let handle = test_executor(dag, ExecutorConfig::default()).start_with_token(&parent_cancellation, "build");

    parent_cancellation.cancel();
    let result = handle.wait().await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Cancelled(_)));
  }

  #[tokio::test]
  async fn dropping_execution_handle_requests_cancellation() {
    let handle = test_executor(DAG::new(), ExecutorConfig::default()).start("build");
    let cancellation = handle.cancellation_token();

    drop(handle);

    assert!(cancellation.is_cancelled());
  }

  #[tokio::test]
  async fn dropping_execution_wait_requests_cancellation() {
    let handle = test_executor(DAG::new(), ExecutorConfig::default()).start("build");
    let cancellation = handle.cancellation_token();

    let wait = handle.wait();
    drop(wait);

    assert!(cancellation.is_cancelled());
  }

  struct DropSignal(Arc<AtomicBool>);

  impl Drop for DropSignal {
    fn drop(&mut self) {
      self.0.store(true, Ordering::SeqCst);
    }
  }

  struct RejectDiagnostics;

  impl octa_output::ConsoleRenderer for RejectDiagnostics {
    fn render(&mut self, entry: &octa_output::ConsoleEntry) -> std::io::Result<()> {
      if matches!(entry.record(), ConsoleRecord::Diagnostic(_)) {
        Err(std::io::Error::other("diagnostic rejected"))
      } else {
        Ok(())
      }
    }
  }

  struct RejectScopeStart;

  impl octa_output::ConsoleRenderer for RejectScopeStart {
    fn render(&mut self, entry: &octa_output::ConsoleEntry) -> std::io::Result<()> {
      if matches!(
        entry.record(),
        ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { .. })
      ) {
        Err(std::io::Error::other("scope start rejected"))
      } else {
        Ok(())
      }
    }
  }

  struct RejectStepStart;

  impl octa_output::ConsoleRenderer for RejectStepStart {
    fn render(&mut self, entry: &octa_output::ConsoleEntry) -> std::io::Result<()> {
      if matches!(
        entry.record(),
        ConsoleRecord::Execution(ExecutionEvent::StepStarted { .. })
      ) {
        Err(std::io::Error::other("step start rejected"))
      } else {
        Ok(())
      }
    }
  }

  #[tokio::test]
  async fn shutdown_timeout_aborts_and_reaps_uncooperative_tasks() {
    let executor = test_executor(DAG::new(), ExecutorConfig::default());
    let dropped = Arc::new(AtomicBool::new(false));
    let signal = DropSignal(dropped.clone());
    let handle = tokio::spawn(async move {
      let _signal = signal;
      std::future::pending::<ExecutorResult<NodeExecution>>().await
    });

    let result = executor
      .shutdown_with_timeout(vec![handle], Duration::from_millis(1))
      .await
      .unwrap();

    assert!(matches!(result.failure, Some(ExecutorError::ShutdownTimeout)));
    assert!(dropped.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn shutdown_log_failure_still_aborts_and_reaps_tasks() {
    let executor = test_executor_with_console(
      DAG::new().into(),
      ExecutorConfig::default(),
      Arc::new(Console::new(RejectDiagnostics)),
      1,
    );
    let dropped = Arc::new(AtomicBool::new(false));
    let signal = DropSignal(dropped.clone());
    let handle = tokio::spawn(async move {
      let _signal = signal;
      std::future::pending::<ExecutorResult<NodeExecution>>().await
    });

    assert!(executor
      .shutdown_with_timeout(vec![handle], Duration::from_secs(1))
      .await
      .is_err());
    assert!(dropped.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn shutdown_results_keep_completed_output_and_record_join_failures() {
    let executor = test_executor(DAG::new(), ExecutorConfig::default());
    let completed = tokio::spawn(async {
      Ok(NodeExecution {
        output: Arc::from("completed"),
        nested_tasks: Vec::new(),
      })
    })
    .await;
    let pending = tokio::spawn(std::future::pending::<ExecutorResult<NodeExecution>>());
    pending.abort();

    let result = executor
      .handle_shutdown_results(vec![completed, pending.await])
      .await
      .unwrap();

    assert_eq!(result.outputs, ["completed"]);
    assert!(matches!(result.failure, Some(ExecutorError::JoinError(_))));
  }

  #[tokio::test]
  async fn normal_completion_records_an_aborted_task_as_a_join_failure() {
    let executor = test_executor(DAG::new(), ExecutorConfig::default());
    let aborted = tokio::spawn(std::future::pending::<ExecutorResult<NodeExecution>>());
    aborted.abort();

    let result = executor.complete_execution(vec![aborted]).await.unwrap();

    assert!(matches!(result.failure, Some(ExecutorError::JoinError(_))));
  }

  #[tokio::test]
  async fn scheduling_reports_a_closed_ready_queue() {
    let mut dag = DAG::new();
    dag.add_node(Arc::new(test_task("task")));
    let executor = test_executor(dag, ExecutorConfig::default());
    let (sender, receiver) = executor.create_task_channel();
    drop(receiver);

    assert!(matches!(
      executor.schedule_initial_tasks(&sender).await,
      Err(ExecutorError::ChannelError)
    ));
  }

  #[tokio::test]
  async fn rejected_scope_start_prevents_the_task_from_running() {
    let completed = Arc::new(AtomicBool::new(false));
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("task");
    let mut task = test_task("task");
    task.completed = completed.clone();
    task.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
    let mut dag = DAG::new();
    dag.add_node(Arc::new(task));
    let executor = test_executor_with_console(
      ExecutionPlan::new(dag, HashMap::new(), vec![scope]),
      ExecutorConfig::default(),
      Arc::new(Console::new(RejectScopeStart)),
      1,
    );

    let result = executor.execute(CancellationToken::new(), "task").await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Failed(_)));
    assert!(!completed.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn rejected_step_start_prevents_the_task_from_running() {
    let completed = Arc::new(AtomicBool::new(false));
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("task");
    let step = allocator.step(&scope, "shell");
    let mut task = test_task("task");
    task.completed = completed.clone();
    task.execution_binding = Some(ExecutionBinding::for_step(scope.clone(), step));
    let mut dag = DAG::new();
    dag.add_node(Arc::new(task));
    let executor = test_executor_with_console(
      ExecutionPlan::new(dag, HashMap::new(), vec![scope]),
      ExecutorConfig::default(),
      Arc::new(Console::new(RejectStepStart)),
      1,
    );

    let result = executor.execute(CancellationToken::new(), "task").await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Failed(_)));
    assert!(!completed.load(Ordering::SeqCst));
  }

  #[test]
  fn rejects_scopes_from_multiple_identity_allocators() {
    let first = octa_output::ConsoleScopeAllocator::default().scope("first");
    let second = octa_output::ConsoleScopeAllocator::default().scope("second");
    let binding = ExecutionBinding::for_task(second);

    assert!(matches!(
      validate_execution_identities(&[first], &[binding]),
      Err(ExecutorError::ExecutionIdentityError(_))
    ));
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
      task.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
      let mut dag = DAG::new();
      dag.add_node(Arc::new(task));
      let events = Arc::new(StdMutex::new(Vec::new()));
      let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
      let summary = Arc::new(Summary::new());
      let plan = ExecutionPlan::new(dag, HashMap::new(), vec![scope.clone()]);
      let executor = test_executor_with_run(
        plan,
        ExecutorConfig {
          summary: Some(summary.clone()),
          ..ExecutorConfig::default()
        },
        console,
        7,
      );

      let result = executor.execute(CancellationToken::new(), "task").await.unwrap();
      assert_eq!(result.conclusion.status(), expected);
      assert_eq!(result.tasks.len(), 1);
      assert_eq!(result.tasks[0].task_id, scope.id());
      assert_eq!(result.tasks[0].conclusion.status(), expected);
      assert!(result.finished_at >= result.started_at);
      assert_eq!(result.failure().is_some(), fails);
      {
        let events = events.lock().unwrap();
        assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
          run_id: 7,
          scope: scope.clone(),
        })));
        assert!(
          events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
            run_id: 7,
            scope: scope.clone(),
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

  #[tokio::test]
  async fn prepare_declares_run_and_scopes_only_once_before_execution() {
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("task");
    let mut task = test_task("task");
    task.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
    let mut dag = DAG::new();
    dag.add_node(Arc::new(task));
    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
    let executor = test_executor_with_run(
      ExecutionPlan::new(dag, HashMap::new(), vec![scope]),
      ExecutorConfig::default(),
      console,
      1,
    );

    executor.prepare("task").await.unwrap();
    executor.prepare("task").await.unwrap();
    executor.execute(CancellationToken::new(), "task").await.unwrap();

    let events = events.lock().unwrap();
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, ConsoleRecord::Execution(ExecutionEvent::RunStarted { .. })))
        .count(),
      1
    );
    assert_eq!(
      events
        .iter()
        .filter(|event| matches!(event, ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { .. })))
        .count(),
      1
    );
  }

  #[tokio::test]
  async fn prepared_executor_rejects_a_different_command() {
    let console = Arc::new(Console::default());
    let executor = test_executor_with_run(ExecutionPlan::from(DAG::new()), ExecutorConfig::default(), console, 1);

    executor.prepare("first").await.unwrap();
    assert!(matches!(
      executor.prepare("second").await,
      Err(ExecutorError::ExecutionIdentityError(message))
        if message.contains("prepared for command 'first'")
    ));
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

  #[derive(Clone)]
  struct RejectScopeDeclaration(Arc<StdMutex<Vec<ConsoleRecord>>>);

  impl octa_output::ConsoleRenderer for RejectScopeDeclaration {
    fn render(&mut self, entry: &octa_output::ConsoleEntry) -> std::io::Result<()> {
      self.0.lock().unwrap().push(entry.record().clone());
      if matches!(
        entry.record(),
        ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { .. })
      ) {
        return Err(std::io::Error::other("scope declaration failed"));
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
    barrier.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
    let mut dag = DAG::new();
    dag.add_node(Arc::new(barrier));
    let plan = ExecutionPlan::new(dag, HashMap::from([("barrier".to_owned(), action)]), vec![scope]);
    let console = Arc::new(Console::new(RejectFirstScope(AtomicBool::new(false))));
    let executor = test_executor_with_console(plan, ExecutorConfig::default(), console, 1);

    assert!(executor.execute(CancellationToken::new(), "main").await.is_err());
    assert!(!completed.load(Ordering::SeqCst));
  }

  #[tokio::test]
  async fn deferred_cleanup_failures_are_reported_without_replacing_the_main_result() {
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("cleanup");
    let mut cleanup = test_task("cleanup");
    cleanup.fails = true;
    cleanup.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
    let mut cleanup_dag = DAG::new();
    cleanup_dag.add_node(Arc::new(cleanup));
    let action = Arc::new(DeferredAction {
      command: "cleanup".to_owned(),
      plan: ExecutionPlan::new(cleanup_dag, HashMap::new(), vec![scope]),
      order: 0,
      registered_after: Vec::new(),
    });
    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
    let mut main_dag = DAG::new();
    main_dag.add_node(Arc::new(test_task("main")));
    let executor = test_executor_with_run(
      ExecutionPlan::new(
        main_dag,
        HashMap::from([("cleanup-barrier".to_owned(), action)]),
        Vec::new(),
      ),
      ExecutorConfig::default(),
      console,
      1,
    );

    let result = executor.execute(CancellationToken::new(), "main").await.unwrap();

    assert!(result.is_success());
    assert_eq!(result.tasks.len(), 1);
    assert_eq!(result.tasks[0].role, TaskRole::Deferred);
    assert!(matches!(result.tasks[0].conclusion, ExecutionConclusion::Failed(_)));
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
  async fn deferred_lifecycle_failures_are_reported_and_propagated() {
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("cleanup");
    let mut cleanup = test_task("cleanup");
    cleanup.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
    let mut cleanup_dag = DAG::new();
    cleanup_dag.add_node(Arc::new(cleanup));
    let action = Arc::new(DeferredAction {
      command: "cleanup".to_owned(),
      plan: ExecutionPlan::new(cleanup_dag, HashMap::new(), vec![scope]),
      order: 0,
      registered_after: Vec::new(),
    });
    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RejectScopeDeclaration(events.clone())));
    let executor = test_executor_with_console(
      ExecutionPlan::new(DAG::new(), HashMap::from([("cleanup".to_owned(), action)]), Vec::new()),
      ExecutorConfig::default(),
      console,
      1,
    );

    assert!(executor.execute(CancellationToken::new(), "main").await.is_err());

    let events = events.lock().unwrap();
    assert!(
      events.iter().any(|event| matches!(
        event,
        ConsoleRecord::Diagnostic(ConsoleDiagnostic { message, .. })
          if message.contains("Deferred command failed")
      )),
      "{events:?}"
    );
  }

  #[tokio::test]
  async fn cancellation_is_reflected_in_scope_and_run_events() {
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let mut task = test_task("build");
    task.cancelled = true;
    task.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
    let mut dag = DAG::new();
    dag.add_node(Arc::new(task));
    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
    let executor = test_executor_with_run(
      ExecutionPlan::new(dag, HashMap::new(), vec![scope.clone()]),
      ExecutorConfig::default(),
      console,
      1,
    );
    let result = executor.execute(CancellationToken::new(), "build").await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Cancelled(_)));
    assert_eq!(result.failure().unwrap().task_id, Some(scope.id()));
    let events = events.lock().unwrap();
    assert!(
      events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
        run_id: executor.context.runtime.run_id,
        scope,
        status: ConsoleStatus::Cancelled,
      }))
    );
    assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::RunFinished {
      run_id: executor.context.runtime.run_id,
      command: "build".to_owned(),
      status: ConsoleStatus::Cancelled,
    })));
  }

  async fn execute_parallel_failure(
    executor_failfast: bool,
    task_failfast: bool,
  ) -> (ExecutorResult<ExecutionResult>, bool) {
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
    let result = result.unwrap();

    assert!(matches!(
      result.conclusion,
      ExecutionConclusion::Failed(ExecutionFailure {
        kind: crate::ExecutionFailureKind::Task,
        ..
      })
    ));
    assert_eq!(result.outputs, ["slow"]);
    assert!(completed);
  }

  #[tokio::test]
  async fn dependency_blocked_by_failure_is_reported_as_skipped() {
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let failed_scope = allocator.scope("failed");
    let blocked_scope = allocator.scope("blocked");
    let mut failed = test_task("failed");
    failed.fails = true;
    failed.execution_binding = Some(ExecutionBinding::for_task(failed_scope.clone()));
    let mut blocked = test_task("blocked");
    blocked.execution_binding = Some(ExecutionBinding::for_task(blocked_scope.clone()));
    let failed = Arc::new(failed);
    let blocked = Arc::new(blocked);
    let mut dag = DAG::new();
    dag.add_node(failed.clone());
    dag.add_node(blocked.clone());
    dag.add_dependency(&failed, &blocked).unwrap();
    let plan = ExecutionPlan::new(dag, HashMap::new(), vec![failed_scope, blocked_scope.clone()]);
    let executor = test_executor_with_console(plan, ExecutorConfig::default(), Arc::new(Console::default()), 1);

    let result = executor.execute(CancellationToken::new(), "failed").await.unwrap();
    let blocked = result
      .tasks
      .iter()
      .find(|task| task.task_id == blocked_scope.id())
      .unwrap();

    assert_eq!(blocked.started_at, None);
    assert_eq!(blocked.conclusion, ExecutionConclusion::Skipped);
  }

  #[tokio::test]
  async fn failfast_cancels_running_tasks_from_config_or_task() {
    for (executor_failfast, task_failfast) in [(true, false), (false, true)] {
      let (result, completed) = execute_parallel_failure(executor_failfast, task_failfast).await;

      assert!(matches!(
        result.unwrap().conclusion,
        ExecutionConclusion::Failed(ExecutionFailure {
          kind: crate::ExecutionFailureKind::Task,
          ..
        })
      ));
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
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("task");
    let mut dag = DAG::new();
    let mut task = test_task("task");
    task.execution_binding = Some(ExecutionBinding::for_task(scope.clone()));
    dag.add_node(Arc::new(task));
    let concurrency = Arc::new(Semaphore::new(1));
    concurrency.close();
    let executor = test_executor_with_console(
      ExecutionPlan::new(dag, HashMap::new(), vec![scope]),
      ExecutorConfig {
        concurrency: Some(concurrency),
        ..ExecutorConfig::default()
      },
      Arc::new(Console::default()),
      1,
    );

    let result = executor.execute(CancellationToken::new(), "test").await;

    assert!(matches!(
      result.unwrap().conclusion,
      ExecutionConclusion::Failed(ExecutionFailure {
        kind: crate::ExecutionFailureKind::Infrastructure,
        ..
      })
    ));
  }

  #[tokio::test]
  async fn cancellation_interrupts_waiting_for_concurrency_permit() {
    let completed = Arc::new(AtomicBool::new(false));
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("task");
    let step = allocator.step(&scope, "shell");
    let mut dag = DAG::new();
    let mut task = test_task("task");
    task.completed = completed.clone();
    task.execution_binding = Some(ExecutionBinding::for_step(scope.clone(), step.clone()));
    dag.add_node(Arc::new(task));
    let concurrency = Arc::new(Semaphore::new(1));
    let _permit = concurrency.clone().acquire_owned().await.unwrap();
    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer { events: events.clone() }));
    let executor = test_executor_with_console(
      ExecutionPlan::new(dag, HashMap::new(), vec![scope.clone()]),
      ExecutorConfig {
        concurrency: Some(concurrency),
        ..ExecutorConfig::default()
      },
      console,
      1,
    );
    let run_id = executor.context.runtime.run_id;
    let cancel_token = CancellationToken::new();
    let execution_token = cancel_token.clone();
    let execution = tokio::spawn(async move { executor.execute(execution_token, "test").await });
    tokio::time::timeout(Duration::from_secs(1), async {
      loop {
        if events.lock().unwrap().iter().any(|event| {
          matches!(
            event,
            ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope: started, .. }) if started == &scope
          )
        }) {
          break;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
    cancel_token.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), execution)
      .await
      .unwrap()
      .unwrap();

    let result = result.unwrap();
    assert!(matches!(result.conclusion, ExecutionConclusion::Cancelled(_)));
    assert_eq!(result.tasks.len(), 1);
    assert_eq!(result.tasks[0].steps.len(), 1);
    assert_eq!(result.tasks[0].steps[0].started_at, None);
    assert!(matches!(
      result.tasks[0].steps[0].conclusion,
      ExecutionConclusion::Cancelled(_)
    ));
    assert!(!completed.load(Ordering::SeqCst));
    let events = events.lock().unwrap();
    assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::StepDeclared {
      run_id,
      scope: scope.clone(),
      step: step.clone(),
    })));
    assert!(!events.iter().any(|event| matches!(
      event,
      ConsoleRecord::Execution(ExecutionEvent::StepStarted { step: started, .. }) if started == &step
    )));
    assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::StepFinished {
      run_id,
      scope,
      step,
      status: ConsoleStatus::Cancelled,
    })));
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

    assert!(matches!(
      result.unwrap().conclusion,
      ExecutionConclusion::Failed(ExecutionFailure {
        kind: crate::ExecutionFailureKind::Infrastructure,
        ..
      })
    ));
  }

  #[test]
  fn originating_error_replaces_secondary_cancellation() {
    let mut recorded = Some(ExecutorError::TaskCancelled("sibling".to_string()));

    record_execution_error(&mut recorded, ExecutorError::TaskFailed("origin".to_string()));
    record_execution_error(&mut recorded, ExecutorError::TaskCancelled("later".to_string()));

    assert!(matches!(recorded, Some(ExecutorError::TaskFailed(task)) if task == "origin"));
  }
}
