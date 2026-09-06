//! Owned handle for a running execution.
//!
//! The handle exposes cooperative cancellation and resolves exactly once to a
//! structured terminal result. Dropping either the handle or its wait future
//! requests cancellation so detached runs are not silently leaked.

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{error::ExecutorResult, execution_result::ExecutionResult};

/// A running executor invocation with cooperative cancellation and an explicit terminal result.
#[must_use = "dropping an execution handle requests cancellation; call wait to obtain its result"]
#[derive(Debug)]
pub struct ExecutionHandle {
  run_id: u64,
  command: String,
  cancellation: CancellationToken,
  task: Option<JoinHandle<ExecutorResult<ExecutionResult>>>,
}

impl ExecutionHandle {
  pub(crate) fn new(
    run_id: u64,
    command: String,
    cancellation: CancellationToken,
    task: JoinHandle<ExecutorResult<ExecutionResult>>,
  ) -> Self {
    Self {
      run_id,
      command,
      cancellation,
      task: Some(task),
    }
  }

  /// Execution-local identifier shared with runtime events and the terminal result.
  pub fn run_id(&self) -> u64 {
    self.run_id
  }

  /// Command supplied when the execution was started.
  pub fn command(&self) -> &str {
    &self.command
  }

  /// Returns a token that can be composed with cancellation owned by an embedding application.
  pub fn cancellation_token(&self) -> CancellationToken {
    self.cancellation.clone()
  }

  /// Requests cooperative cancellation. Calling this more than once is harmless.
  pub fn cancel(&self) {
    self.cancellation.cancel();
  }

  /// Returns whether the spawned execution future has completed.
  pub fn is_finished(&self) -> bool {
    self.task.as_ref().is_none_or(JoinHandle::is_finished)
  }

  /// Waits for the authoritative terminal result.
  pub async fn wait(mut self) -> ExecutorResult<ExecutionResult> {
    let result = self
      .task
      .as_mut()
      .expect("execution task is available until the handle is consumed")
      .await?;
    self.task.take();
    result
  }

  /// Requests cancellation and waits for cleanup and the terminal result.
  pub async fn cancel_and_wait(self) -> ExecutorResult<ExecutionResult> {
    self.cancel();
    self.wait().await
  }
}

impl Drop for ExecutionHandle {
  fn drop(&mut self) {
    if self.task.is_some() {
      self.cancellation.cancel();
    }
  }
}
