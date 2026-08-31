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
use octa_plugin_manager::plugin_manager::PluginManager;
use sled::Db;
use tokio::{
  select,
  sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
  task::JoinHandle,
  time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::{
  error::{ExecutorError, ExecutorResult},
  summary::{Summary, TaskSummaryItem},
  task::{CacheItem, Executable, TaskItem},
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
  pub(crate) fn new(dag: DAG<T>, deferred: HashMap<String, Arc<DeferredAction<T>>>) -> Self {
    Self { dag, deferred }
  }
}

impl<T: Eq + Hash + Identifiable> From<DAG<T>> for ExecutionPlan<T> {
  /// Keeps plain DAG execution available for callers that do not use deferred actions.
  fn from(dag: DAG<T>) -> Self {
    Self {
      dag,
      deferred: HashMap::new(),
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
  pub silent: bool,
  /// Cancel tasks that are already running when any task in the plan fails.
  pub failfast: bool,
  /// Shared limiter for concurrently executing non-internal tasks.
  pub concurrency: Option<Arc<Semaphore>>,
}

impl Default for ExecutorConfig {
  fn default() -> Self {
    Self {
      silent: true,
      failfast: false,
      concurrency: None,
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
    })
  }

  /// Executes all tasks in the DAG
  pub async fn execute(&self, cancel_token: CancellationToken, command: &str) -> ExecutorResult<Vec<String>> {
    self.log_info(&format!("Starting execution plan for command {}", command));

    self.initialize_execution().await?;
    let (tx, rx) = self.create_task_channel();
    let mut handles = Vec::with_capacity(self.state.dag.node_count());
    // Internal cancellation must not cancel the caller's token: the caller may reuse it for
    // another top-level task or a subsequent watch iteration.
    let execution_token = cancel_token.child_token();

    self.schedule_initial_tasks(&tx).await?;

    let result = match self.process_tasks(execution_token.clone(), rx, &tx, &mut handles).await {
      Ok(_) => self.handle_completion(cancel_token, handles).await,
      Err(error) => {
        execution_token.cancel();
        Err(error)
      },
    };

    self.run_deferred().await;
    result
  }

  async fn initialize_execution(&self) -> ExecutorResult<()> {
    self.initialize_in_degrees().await
  }

  fn create_task_channel(&self) -> (mpsc::Sender<Arc<T>>, mpsc::Receiver<Arc<T>>) {
    mpsc::channel(self.state.dag.node_count())
  }

  async fn run_deferred(&self) {
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
        self.plugin_manager.clone(),
        self.state.cache.clone(),
        self.state.fingerprint.clone(),
        self.state.dry,
        self.state.force,
        self.config.concurrency.clone(),
      )
      .await
      {
        error!("Deferred command failed: {}", error);
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
      completed_tasks: self.state.completed_tasks.clone(),
      deferred: self.deferred.clone(),
    };

    let plugin_manager = Arc::clone(&self.plugin_manager);

    tokio::spawn(async move {
      TaskExecutor::new(executor_state, task, tx, cancel_token, plugin_manager)
        .execute()
        .await
    })
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

    self.log_info("All tasks completed successfully");

    Ok(results)
  }

  async fn shutdown(&self, handles: Vec<JoinHandle<ExecutorResult<String>>>) -> ExecutorResult<Vec<String>> {
    self.log_info("Initiating graceful shutdown");

    match timeout(SHUTDOWN_TIMEOUT, join_all(handles)).await {
      Ok(results) => self.handle_shutdown_results(results),
      Err(_) => {
        error!("Shutdown timeout exceeded, forcing shutdown");
        Err(ExecutorError::ShutdownTimeout)
      },
    }
  }

  fn handle_shutdown_results(
    &self,
    results: Vec<Result<ExecutorResult<String>, tokio::task::JoinError>>,
  ) -> ExecutorResult<Vec<String>> {
    for result in results {
      if let Err(e) = result.map_err(ExecutorError::JoinError)? {
        error!("Task failed during shutdown: {}", e);
      }
    }
    self.log_info("Graceful shutdown completed");
    Ok(vec![])
  }

  fn log_info(&self, message: &str) {
    if !self.config.silent {
      info!("{}", message);
    }
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
  completed_tasks: Arc<Mutex<HashSet<String>>>,
  deferred: Arc<HashMap<String, Arc<DeferredAction<T>>>>,
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
    let task_name = self.task.id();
    debug!("Executing task: {}", task_name);

    // Internal graph barriers do not represent running user work. Deferred barriers are also
    // internal; their nested executor acquires permits for the actual cleanup commands.
    let _permit = match self.acquire_permit().await {
      Ok(permit) => permit,
      Err(error) => return self.handle_error(error).await,
    };
    let start_time = SystemTime::now();
    let deferred = self.context.deferred.get(&task_name).cloned();
    let result = if let Some(action) = deferred.as_ref() {
      // The graph node is only an ordering barrier; its actual work lives in the nested plan.
      execute_deferred_action(
        action.clone(),
        self.plugin_manager.clone(),
        self.context.cache.clone(),
        self.context.fingerprint.clone(),
        self.context.dry,
        self.context.force,
        self.context.concurrency.clone(),
      )
      .await
    } else {
      self
        .task
        .execute(
          self.plugin_manager.clone(),
          self.context.cache.clone(),
          self.context.fingerprint.clone(),
          self.context.dry,
          self.context.force,
          self.cancel_token.clone(),
        )
        .await
    };

    match result {
      Ok(output) => self.handle_success(output, start_time).await,
      Err(error) if deferred.is_some() => {
        // Cleanup failures are reported without replacing the main task result.
        error!("Deferred command failed: {}", error);
        self.handle_success(String::new(), start_time).await
      },
      Err(e) => self.handle_error(e).await,
    }
  }

  async fn acquire_permit(&self) -> ExecutorResult<Option<OwnedSemaphorePermit>> {
    if self.task.is_internal() {
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

  async fn handle_success(&self, output: String, start_time: SystemTime) -> ExecutorResult<String> {
    if self.cancel_token.is_cancelled() {
      debug!("Task {} cancelled during execution", self.task.id());
      return Ok(String::from(""));
    }

    if !self.task.is_internal() {
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

  async fn handle_error(&self, error: ExecutorError) -> ExecutorResult<String> {
    error!("Task {} failed: {}", self.task.name(), error);
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
  plugin_manager: Arc<PluginManager>,
  cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
  fingerprint: Arc<Db>,
  dry: bool,
  force: bool,
  concurrency: Option<Arc<Semaphore>>,
) -> ExecutorResult<String> {
  // A fresh token lets cleanup continue even when cancellation stopped the main plan.
  let executor = Executor::new(
    plugin_manager,
    action.plan.clone(),
    ExecutorConfig {
      silent: true,
      failfast: false,
      concurrency,
    },
    Some(cache),
    fingerprint,
    dry,
    force,
    None,
  )?;

  Box::pin(executor.execute(CancellationToken::new(), &action.command))
    .await
    .map(|results| results.join("\n"))
}

#[cfg(test)]
mod tests {
  use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
  };

  use async_trait::async_trait;
  use tempfile::TempDir;
  use tokio::time::sleep;

  use super::*;
  use crate::task::RunMode;

  #[derive(Clone)]
  struct TestTask {
    id: String,
    internal: bool,
    fails: bool,
    failfast: bool,
    completed: Arc<AtomicBool>,
    running: Option<Arc<AtomicUsize>>,
    maximum_running: Option<Arc<AtomicUsize>>,
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
  }

  #[async_trait]
  impl Executable<TestTask> for TestTask {
    async fn execute(
      &self,
      _plugin_manager: Arc<PluginManager>,
      _cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
      _fingerprint: Arc<Db>,
      _dry: bool,
      _force: bool,
      cancel_token: CancellationToken,
    ) -> ExecutorResult<String> {
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

      select! {
        _ = sleep(Duration::from_millis(150)) => {
          self.completed.store(true, Ordering::SeqCst);
          Ok(self.id.clone())
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
      fails: false,
      failfast: false,
      completed: Arc::new(AtomicBool::new(false)),
      running: None,
      maximum_running: None,
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

  #[test]
  fn originating_error_replaces_secondary_cancellation() {
    let mut recorded = Some(ExecutorError::TaskCancelled("sibling".to_string()));

    record_execution_error(&mut recorded, ExecutorError::TaskFailed("origin".to_string()));
    record_execution_error(&mut recorded, ExecutorError::TaskCancelled("later".to_string()));

    assert!(matches!(recorded, Some(ExecutorError::TaskFailed(task)) if task == "origin"));
  }
}
