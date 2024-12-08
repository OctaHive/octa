use std::{
  collections::HashMap,
  hash::Hash,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  },
  time::Duration,
};

use futures::future::join_all;
use octa_dag::{Identifiable, DAG};
use tokio::{
  select,
  sync::{mpsc, Mutex},
  task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::{
  error::{ExecutorError, ExecutorResult},
  task::{Executable, TaskResult},
};

// Add shutdown timeout constant
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Executor manages the execution of tasks in a directed acyclic graph (DAG)
pub struct Executor<T: Eq + Hash + Identifiable + Executable + Send + std::marker::Sync + 'static> {
  dag: Arc<DAG<T>>,                              // Task dependency graph
  in_degree: Arc<Mutex<HashMap<String, usize>>>, // Tracks task dependencies
  active_tasks: Arc<AtomicUsize>,                // Number of running tasks
  finished: CancellationToken,
}

impl<T: Eq + Hash + Identifiable + Executable + Send + std::marker::Sync + 'static> Executor<T> {
  /// Creates a new Executor instance with the given DAG
  pub fn new(dag: DAG<T>) -> Self {
    let in_degree = dag
      .nodes()
      .iter()
      .map(|n| (n.id().clone(), 0))
      .collect::<HashMap<_, _>>();

    Self {
      dag: Arc::new(dag),
      in_degree: Arc::new(Mutex::new(in_degree)),
      active_tasks: Arc::new(AtomicUsize::new(0)),
      finished: CancellationToken::new(),
    }
  }

  // Add graceful shutdown method
  async fn shutdown(&self, handles: Vec<JoinHandle<ExecutorResult<()>>>) -> ExecutorResult<()> {
    info!("Initiating graceful shutdown");

    // Create a timeout future for all handles
    let shutdown_timeout = tokio::time::timeout(SHUTDOWN_TIMEOUT, join_all(handles));

    match shutdown_timeout.await {
      Ok(results) => {
        // Check if any tasks failed during shutdown
        for result in results {
          if let Err(e) = result {
            error!("Task failed during shutdown: {}", e);
          }
        }
        info!("Graceful shutdown completed");
        Ok(())
      },
      Err(_) => {
        error!("Shutdown timeout exceeded, forcing shutdown");
        Err(ExecutorError::ShutdownTimeout)
      },
    }
  }

  /// Executes all tasks in the DAG
  pub async fn execute(&self, cancel_token: CancellationToken) -> ExecutorResult<()> {
    info!("Starting task execution");

    self.initialize_in_degrees().await?;

    let (tx, rx) = mpsc::channel(self.dag.node_count());
    let mut handles = Vec::with_capacity(self.dag.node_count());

    self.schedule_initial_tasks(&tx).await?;

    match self.process_tasks(cancel_token.clone(), rx, &tx, &mut handles).await {
      Ok(_) => {
        if cancel_token.is_cancelled() {
          // Handle graceful shutdown
          self.shutdown(handles).await?;
        } else {
          // Normal completion
          for handle in handles {
            handle.await??;
          }
          info!("All tasks completed successfully");
        }
      },
      Err(e) => {
        error!("Error during task processing: {}", e);
        cancel_token.cancel();
        self.shutdown(handles).await?;
        return Err(e);
      },
    }

    Ok(())
  }

  /// Processes tasks as they become available
  async fn process_tasks(
    &self,
    cancel_token: CancellationToken,
    mut rx: mpsc::Receiver<Arc<T>>,
    tx: &mpsc::Sender<Arc<T>>,
    handles: &mut Vec<JoinHandle<ExecutorResult<()>>>,
  ) -> ExecutorResult<()> {
    while let Some(task) = select! {
      task = rx.recv() => task,
      _ = cancel_token.cancelled() => {
        debug!("Execution cancelled, stop processing task");
        return Ok(());
      }
      _ = self.finished.cancelled() => {
        return Ok(());
      }
    } {
      let handle = self.spawn_task(cancel_token.clone(), task, tx.clone());
      handles.push(handle);
    }

    Ok(())
  }

  /// Spawns a new task execution
  fn spawn_task(
    &self,
    cancel_token: CancellationToken,
    task: Arc<T>,
    tx: mpsc::Sender<Arc<T>>,
  ) -> JoinHandle<ExecutorResult<()>> {
    let dag = self.dag.clone();
    let cancel = cancel_token.clone();
    let finished = self.finished.clone();
    let in_degree = self.in_degree.clone();
    let active_tasks = self.active_tasks.clone();
    let task_name = task.id().clone();

    tokio::spawn(async move {
      debug!("Executing task: {}", task_name);

      let result = task.execute().await;

      match result {
        Ok(output) => {
          if cancel.is_cancelled() {
            debug!("Task {} cancelled during execution", task_name);
            return Ok(());
          }

          Self::process_task_success(&dag, task, output, &finished, &in_degree, &active_tasks, &tx).await?;
        },
        Err(e) => {
          error!("Task {} failed: {}", task_name, e);
          cancel.cancel();
          return Err(e);
        },
      }

      Ok(())
    })
  }

  /// Processes successful task completion and schedules dependent tasks
  async fn process_task_success(
    dag: &DAG<T>,
    task: Arc<T>,
    output: TaskResult,
    finished: &CancellationToken,
    in_degree: &Mutex<HashMap<String, usize>>,
    active_tasks: &AtomicUsize,
    tx: &mpsc::Sender<Arc<T>>,
  ) -> ExecutorResult<()> {
    // Process task completion in DAG
    if let Some(deps) = dag.edges().get(&task.id()) {
      for dep in deps {
        dep.set_result(task.id().clone(), output.clone()).await;
      }
    }

    // Schedule dependent tasks
    if let Some(deps) = dag.edges().get(&task.id()) {
      let mut degrees = in_degree.lock().await;
      for dep in deps {
        let dep_count = degrees
          .get_mut(&dep.id())
          .ok_or_else(|| ExecutorError::TaskNotFound(dep.id().clone()))?;
        *dep_count -= 1;

        if *dep_count == 0 {
          active_tasks.fetch_add(1, Ordering::SeqCst);
          tx.send(dep.clone()).await.map_err(|_| ExecutorError::ChannelError)?;
        }
      }
    }

    // Check if all tasks are complete
    if active_tasks.fetch_sub(1, Ordering::SeqCst) == 1 {
      finished.cancel();
    }

    Ok(())
  }

  /// Schedules tasks with no dependencies
  async fn schedule_initial_tasks(&self, tx: &mpsc::Sender<Arc<T>>) -> ExecutorResult<()> {
    let degrees = self.in_degree.lock().await;
    for node in self.dag.nodes() {
      if degrees[&node.id()] == 0 {
        self.active_tasks.fetch_add(1, Ordering::SeqCst);
        tx.send(node.clone()).await.unwrap();
      }
    }

    Ok(())
  }

  /// Initializes dependency counts for all tasks
  async fn initialize_in_degrees(&self) -> ExecutorResult<()> {
    let mut degrees = self.in_degree.lock().await;
    for deps in self.dag.edges().values() {
      for node in deps {
        *degrees
          .get_mut(&node.id())
          .ok_or_else(|| ExecutorError::TaskNotFound(node.id().clone()))? += 1;
      }
    }
    Ok(())
  }
}
