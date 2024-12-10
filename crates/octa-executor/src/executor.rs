use std::{
  collections::HashMap,
  hash::Hash,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  },
  time::{Duration, SystemTime},
};

use futures::future::join_all;
use indexmap::IndexMap;
use octa_dag::{Identifiable, DAG};
use tokio::{
  select,
  sync::{mpsc, Mutex},
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

/// Configuration for the Executor
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
  pub show_summary: bool,
  pub silent: bool,
}

impl Default for ExecutorConfig {
  fn default() -> Self {
    Self {
      show_summary: false,
      silent: true,
    }
  }
}

/// Tracks the state of task execution
#[derive(Debug)]
struct ExecutionState<T: Hash + Identifiable + Eq + TaskItem> {
  dag: Arc<DAG<T>>,                               // Task dependency graph
  in_degree: Arc<Mutex<HashMap<String, usize>>>,  // Tracks task dependencies
  active_tasks: Arc<AtomicUsize>,                 // Number of running tasks
  summary: Arc<Mutex<Summary>>,                   // Summary of task execution
  cache: Arc<Mutex<IndexMap<String, CacheItem>>>, // Cache for tasks
}

/// Executor manages the execution of tasks in a directed acyclic graph (DAG)
pub struct Executor<T: Eq + Hash + Identifiable + TaskItem + Executable<T> + Send + Sync + Clone + 'static> {
  state: ExecutionState<T>,
  config: ExecutorConfig,
  finished: CancellationToken,
}

impl<T: Eq + Hash + Identifiable + TaskItem + Executable<T> + Send + Sync + Clone + 'static> Executor<T> {
  /// Creates a new Executor instance with the given DAG
  pub fn new(dag: DAG<T>, config: ExecutorConfig, cache: Option<Arc<Mutex<IndexMap<String, CacheItem>>>>) -> Self {
    let in_degree = dag.nodes().iter().map(|n| (n.id().clone(), 0)).collect();

    let cache = match cache {
      Some(cache) => cache,
      None => Arc::new(Mutex::new(IndexMap::new())),
    };

    let state = ExecutionState {
      dag: Arc::new(dag),
      in_degree: Arc::new(Mutex::new(in_degree)),
      active_tasks: Arc::new(AtomicUsize::new(0)),
      summary: Arc::new(Mutex::new(Summary::new())),
      cache,
    };

    Self {
      state,
      config,
      finished: CancellationToken::new(),
    }
  }

  /// Executes all tasks in the DAG
  pub async fn execute(&self, cancel_token: CancellationToken) -> ExecutorResult<Vec<String>> {
    self.log_info("Starting task execution");

    self.initialize_execution().await?;
    let (tx, rx) = self.create_task_channel();
    let mut handles = Vec::with_capacity(self.state.dag.node_count());

    self.schedule_initial_tasks(&tx).await?;

    match self.process_tasks(cancel_token.clone(), rx, &tx, &mut handles).await {
      Ok(_) => self.handle_completion(cancel_token, handles).await,
      Err(e) => self.handle_error(e, cancel_token).await,
    }
  }

  async fn initialize_execution(&self) -> ExecutorResult<()> {
    self.initialize_in_degrees().await
  }

  fn create_task_channel(&self) -> (mpsc::Sender<Arc<T>>, mpsc::Receiver<Arc<T>>) {
    mpsc::channel(self.state.dag.node_count())
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

  async fn handle_error(&self, error: ExecutorError, cancel_token: CancellationToken) -> ExecutorResult<Vec<String>> {
    error!("Error during task processing: {}", error);
    cancel_token.cancel();
    Err(error)
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
    };

    tokio::spawn(async move {
      TaskExecutor::new(executor_state, task, tx, cancel_token)
        .execute()
        .await
    })
  }

  /// Schedules tasks with no dependencies
  async fn schedule_initial_tasks(&self, tx: &mpsc::Sender<Arc<T>>) -> ExecutorResult<()> {
    let degrees = self.state.in_degree.lock().await;
    for node in self.state.dag.nodes() {
      if degrees[&node.id()] == 0 {
        self.state.active_tasks.fetch_add(1, Ordering::SeqCst);
        tx.send(node.clone()).await.unwrap();
      }
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

    for handle in handles {
      results.push(handle.await??);
    }

    self.log_info("All tasks completed successfully");

    if self.config.show_summary {
      let summary = self.state.summary.lock().await;
      summary.print();
    }

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
      if let Err(e) = result.map_err(|e| ExecutorError::JoinError(e))? {
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
  summary: Arc<Mutex<Summary>>,
  cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
}

struct TaskExecutor<T: Executable<T> + Identifiable + TaskItem + Hash + Eq + Clone + 'static> {
  context: ExecutorContext<T>,
  task: Arc<T>,
  tx: mpsc::Sender<Arc<T>>,
  cancel_token: CancellationToken,
}

impl<T: Executable<T> + Identifiable + TaskItem + Hash + Eq + Clone + 'static> TaskExecutor<T> {
  fn new(context: ExecutorContext<T>, task: Arc<T>, tx: mpsc::Sender<Arc<T>>, cancel_token: CancellationToken) -> Self {
    Self {
      context,
      task,
      tx,
      cancel_token,
    }
  }

  async fn execute(self) -> ExecutorResult<String> {
    let task_name = self.task.id();
    debug!("Executing task: {}", task_name);

    let start_time = SystemTime::now();
    let result = self.task.execute(self.context.cache.clone()).await;

    match result {
      Ok(output) => self.handle_success(output, start_time).await,
      Err(e) => self.handle_error(e).await,
    }
  }

  async fn handle_success(&self, output: String, start_time: SystemTime) -> ExecutorResult<String> {
    if self.cancel_token.is_cancelled() {
      debug!("Task {} cancelled during execution", self.task.id());
      return Ok(String::from(""));
    }

    if let Ok(elapsed) = start_time.elapsed() {
      self.context.summary.lock().await.add(TaskSummaryItem {
        name: self.task.id(),
        duration: elapsed,
      });
    }

    self.process_task_success(output).await
  }

  async fn handle_error(&self, error: ExecutorError) -> ExecutorResult<String> {
    error!("Task {} failed: {}", self.task.id(), error);
    self.context.finished.cancel();
    Err(error)
  }

  async fn process_task_success(&self, output: String) -> ExecutorResult<String> {
    if let Some(deps) = self.context.dag.edges().get(&self.task.id()) {
      for dep in deps {
        dep.set_result(self.task.id(), output.clone()).await;
      }

      let mut degrees = self.context.in_degree.lock().await;
      for dep in deps {
        let dep_count = degrees
          .get_mut(&dep.id())
          .ok_or_else(|| ExecutorError::TaskNotFound(dep.id()))?;
        *dep_count -= 1;

        if *dep_count == 0 {
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
