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

use chrono::{DateTime, Utc};
use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
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
  execution_result::{conclusion, ExecutionFailure, ExecutionResult, TaskResult, TaskRole},
  interactive_scope_tracker::InteractiveScopeTracker,
  runtime_coordinator::RuntimeCoordinator,
  summary::{Summary, TaskSummaryItem},
  task::{CacheItem, Executable, ExecutionBinding, TaskItem, TaskOutcome, TaskRuntime},
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

struct PlanExecution {
  outputs: Vec<String>,
  nested_tasks: Vec<TaskResult>,
  failure: Option<ExecutorError>,
}

struct NodeExecution {
  output: String,
  nested_tasks: Vec<TaskResult>,
}

#[derive(Default)]
struct RunLifecycle {
  command: Option<String>,
  started_at: Option<DateTime<Utc>>,
}

fn mark_deferred(tasks: &mut [TaskResult]) {
  for task in tasks {
    task.role = TaskRole::Deferred;
  }
}

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
  // Successful normal nodes determine which deferred actions were registered before interruption;
  // deferred barriers are also recorded when attempted so cleanup is never executed twice.
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
  run_lifecycle: Mutex<RunLifecycle>,
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
    let node_bindings = dag
      .nodes()
      .iter()
      .filter_map(|node| node.execution_binding())
      .collect::<Vec<_>>();
    validate_execution_identities(&plan.scopes, &node_bindings)?;
    let run_id = config.run_id.unwrap_or_else(|| config.console.allocate_run_id());
    let scope_tracker = Arc::new(ConsoleScopeTracker::new(
      config.console.clone(),
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
      run_lifecycle: Mutex::new(RunLifecycle::default()),
    })
  }

  /// Executes all tasks in the DAG and returns their complete terminal state.
  ///
  /// Task, command, plugin, timeout, and cancellation failures are represented by
  /// [`ExecutionResult::conclusion`]. An [`ExecutorError`] is returned only when a complete
  /// result cannot be formed or its terminal lifecycle cannot be published.
  pub async fn execute(&self, cancel_token: CancellationToken, command: &str) -> ExecutorResult<ExecutionResult> {
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
      },
    };
    let deferred_exit_code = plan.failure.as_ref().and_then(ExecutorError::command_exit_code);
    self.interactive_tracker.finish_remaining().await;
    let deferred_result = self.run_deferred(deferred_exit_code).await;

    let mut status = if deferred_result.is_err() {
      ConsoleStatus::Failed
    } else {
      match &plan.failure {
        None => self.scope_tracker.successful_run_status().await,
        Some(ExecutorError::TaskCancelled(_)) => ConsoleStatus::Cancelled,
        Some(_) => ConsoleStatus::Failed,
      }
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
    finish_result?;
    run_result?;
    let deferred_tasks = deferred_result?;

    let mut tasks = self.scope_tracker.results().await?;
    tasks.extend(plan.nested_tasks);
    tasks.extend(deferred_tasks);
    tasks.sort_by_key(|task| task.task_id);
    let failure = self.scope_tracker.failure().await.or_else(|| {
      plan
        .failure
        .as_ref()
        .map(|error| ExecutionFailure::from_error(error, None))
    });
    Ok(ExecutionResult {
      run_id: self.run_id,
      command: command.to_owned(),
      started_at,
      finished_at: Utc::now(),
      conclusion: conclusion(status, failure, None, None),
      tasks,
      outputs: plan.outputs,
    })
  }

  /// Publishes run and scope declarations before a later call to [`Self::execute`].
  ///
  /// Batch schedulers can prepare executors in declaration order and then execute them
  /// concurrently. Calling this more than once with the same command is idempotent. A prepared
  /// executor is single-use and must subsequently be passed to [`Self::execute`].
  pub async fn prepare(&self, command: &str) -> ExecutorResult<()> {
    if let Err(error) = self.begin_execution(command).await {
      self.finish_failed_start(command).await;
      return Err(error);
    }
    Ok(())
  }

  async fn begin_execution(&self, command: &str) -> ExecutorResult<DateTime<Utc>> {
    let mut lifecycle = self.run_lifecycle.lock().await;
    if let Some(existing) = &lifecycle.command {
      if existing != command {
        return Err(ExecutorError::ExecutionIdentityError(format!(
          "executor was prepared for command '{existing}', not '{command}'"
        )));
      }
    } else {
      let started_at = Utc::now();
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
      lifecycle.command = Some(command.to_owned());
      lifecycle.started_at = Some(started_at);
    }
    self.scope_tracker.declare().await?;
    Ok(
      lifecycle
        .started_at
        .expect("run start is recorded before scope declaration"),
    )
  }

  async fn finish_failed_start(&self, command: &str) {
    let _ = self.scope_tracker.finish_remaining(ConsoleStatus::Failed).await;
    if self.config.emit_run_events {
      let command = self
        .run_lifecycle
        .lock()
        .await
        .command
        .clone()
        .unwrap_or_else(|| command.to_owned());
      let _ = self
        .config
        .console
        .event(ExecutionEvent::RunFinished {
          run_id: self.run_id,
          command,
          status: ConsoleStatus::Failed,
        })
        .await;
    }
  }

  async fn execute_plan(&self, cancel_token: CancellationToken) -> ExecutorResult<PlanExecution> {
    self.initialize_execution().await?;
    let (tx, rx) = self.create_task_channel();
    let mut handles = Vec::with_capacity(self.state.dag.node_count());
    // Internal cancellation must not cancel the caller's token: the caller may reuse it for
    // another top-level task or a subsequent watch iteration.
    let execution_token = cancel_token.child_token();

    self.schedule_initial_tasks(&tx).await?;
    self.process_tasks(execution_token, rx, &tx, &mut handles).await;
    self.handle_completion(cancel_token, handles).await
  }

  async fn initialize_execution(&self) -> ExecutorResult<()> {
    self.initialize_in_degrees().await
  }

  fn create_task_channel(&self) -> (mpsc::Sender<Arc<T>>, mpsc::Receiver<Arc<T>>) {
    mpsc::channel(self.state.dag.node_count().max(1))
  }

  async fn run_deferred(&self, exit_code: Option<i32>) -> ExecutorResult<Vec<TaskResult>> {
    let completed_tasks = self.state.completed_tasks.lock().await.clone();
    let mut tasks = Vec::new();
    let mut first_error = None;
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

      let result = execute_deferred_action(
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
      .await;
      match result {
        Ok(mut result) => {
          let failure = result.failure().map(ToString::to_string);
          mark_deferred(&mut result.tasks);
          tasks.extend(result.tasks);
          if let Some(failure) = failure {
            if let Err(error) = self
              .config
              .console
              .run_message(
                self.run_id,
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
            .config
            .console
            .run_message(
              self.run_id,
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

  /// Processes tasks as they become available
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
        _ = self.finished.cancelled() => None
    }
  }

  /// Spawns a new task execution
  fn spawn_task(
    &self,
    cancel_token: CancellationToken,
    task: Arc<T>,
    tx: mpsc::Sender<Arc<T>>,
  ) -> JoinHandle<ExecutorResult<NodeExecution>> {
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
      outputs: indexed_outputs.into_iter().map(|(_, output)| output).collect(),
      nested_tasks,
      failure: first_error,
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
          outputs.push(result.output);
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
    })
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
      if let Err(error) = self.context.scope_tracker.start_scope(binding).await {
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
      if let Err(error) = self.context.scope_tracker.start_step(binding).await {
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
    let deferred = self.context.deferred.get(&task_name).cloned();
    let mut nested_tasks = Vec::new();
    let result = if let Some(action) = deferred.as_ref() {
      // Reaching the barrier consumes this cleanup action even when its nested lifecycle fails;
      // shutdown cleanup must never execute the same action a second time.
      self.context.completed_tasks.lock().await.insert(task_name.clone());
      // The graph node is only an ordering barrier; its actual work lives in the nested plan.
      match execute_deferred_action(
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
      {
        Ok(mut result) => {
          let failure = result.failure().map(ToString::to_string);
          let output = result.outputs.join("\n");
          mark_deferred(&mut result.tasks);
          nested_tasks = result.tasks;
          if let Some(failure) = failure {
            match self
              .context
              .console
              .run_message(
                self.context.run_id,
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
  ) -> ExecutorResult<String> {
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
    let completion = self.context.scope_tracker.complete(binding, status, failure).await;
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

    // Regular successful nodes register later deferred actions. Deferred barriers may already
    // have been recorded before their nested plan ran, so insertion is intentionally idempotent.
    self.context.completed_tasks.lock().await.insert(self.task.id());

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
          .console
          .run_message_at(
            self.context.run_id,
            binding.scope().clone(),
            ConsoleLevel::Error,
            message,
          )
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
) -> ExecutorResult<ExecutionResult> {
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
  use crate::task::RunMode;
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

    fn execution_binding(&self) -> Option<ExecutionBinding> {
      self.execution_binding.clone()
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
    let executor = test_executor(
      DAG::new(),
      ExecutorConfig {
        emit_run_events: true,
        console: Arc::new(Console::new(RejectDiagnostics)),
        ..ExecutorConfig::default()
      },
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
        output: "completed".to_owned(),
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
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      ExecutionPlan::new(dag, HashMap::new(), vec![scope]),
      ExecutorConfig {
        emit_run_events: true,
        console: Arc::new(Console::new(RecordingRenderer { events: events.clone() })),
        ..ExecutorConfig::default()
      },
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap();

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
    let executor = test_executor(DAG::new(), ExecutorConfig::default());

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
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      ExecutionPlan::new(
        main_dag,
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
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      ExecutionPlan::new(DAG::new(), HashMap::from([("cleanup".to_owned(), action)]), Vec::new()),
      ExecutorConfig {
        console: Arc::new(Console::new(RejectScopeDeclaration(events.clone()))),
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
    let result = executor.execute(CancellationToken::new(), "build").await.unwrap();

    assert!(matches!(result.conclusion, ExecutionConclusion::Cancelled(_)));
    assert_eq!(result.failure().unwrap().task_id, Some(scope.id()));
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
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      plan,
      ExecutorConfig::default(),
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap();

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
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      ExecutionPlan::new(dag, HashMap::new(), vec![scope]),
      ExecutorConfig {
        concurrency: Some(concurrency),
        ..ExecutorConfig::default()
      },
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap();

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
    let executor = Executor::new(
      Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
      ExecutionPlan::new(dag, HashMap::new(), vec![scope.clone()]),
      ExecutorConfig {
        concurrency: Some(concurrency),
        console: Arc::new(Console::new(RecordingRenderer { events: events.clone() })),
        ..ExecutorConfig::default()
      },
      None,
      Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      false,
      false,
      None,
    )
    .unwrap();
    let run_id = executor.run_id;
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
