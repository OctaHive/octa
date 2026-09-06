//! Top-level run lifecycle and event publication.
//!
//! A run may be prepared and started through different API paths, so start and
//! finish transitions are idempotent for the same command and terminal status.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use octa_output::{Console, ConsoleStatus, ExecutionEvent};
use tokio::sync::Mutex;

use crate::error::{ExecutorError, ExecutorResult};

#[derive(Default)]
struct RunState {
  command: Option<String>,
  started_at: Option<DateTime<Utc>>,
  finished_at: Option<DateTime<Utc>>,
  status: Option<ConsoleStatus>,
}

/// Owns the identity and top-level lifecycle of one execution.
pub(crate) struct ExecutionRun {
  console: Arc<Console>,
  run_id: u64,
  transitions: Mutex<()>,
  state: Mutex<RunState>,
}

impl ExecutionRun {
  pub(crate) fn new(console: Arc<Console>, run_id: u64) -> Self {
    Self {
      console,
      run_id,
      transitions: Mutex::new(()),
      state: Mutex::new(RunState::default()),
    }
  }

  pub(crate) async fn start(&self, command: &str) -> ExecutorResult<DateTime<Utc>> {
    let _transition = self.transitions.lock().await;
    {
      let state = self.state.lock().await;
      validate_command(&state, command)?;
      if let Some(started_at) = state.started_at {
        return Ok(started_at);
      }
    }

    let started_at = Utc::now();
    self
      .console
      .event(ExecutionEvent::RunStarted {
        run_id: self.run_id,
        command: command.to_owned(),
      })
      .await?;
    let mut state = self.state.lock().await;
    state.command = Some(command.to_owned());
    state.started_at = Some(started_at);
    Ok(started_at)
  }

  pub(crate) async fn finish(&self, command: &str, status: ConsoleStatus) -> ExecutorResult<DateTime<Utc>> {
    let _transition = self.transitions.lock().await;
    {
      let state = self.state.lock().await;
      validate_command(&state, command)?;
      if let Some(finished_at) = state.finished_at {
        if state.status == Some(status) {
          return Ok(finished_at);
        }
        return Err(ExecutorError::ExecutionIdentityError(format!(
          "execution '{}' was already finished with status {:?}",
          command,
          state.status.expect("finished executions record their status")
        )));
      }
    }

    self
      .console
      .event(ExecutionEvent::RunFinished {
        run_id: self.run_id,
        command: command.to_owned(),
        status,
      })
      .await?;
    let finished_at = Utc::now();
    let mut state = self.state.lock().await;
    state.finished_at = Some(finished_at);
    state.status = Some(status);
    Ok(finished_at)
  }
}

fn validate_command(state: &RunState, command: &str) -> ExecutorResult<()> {
  match &state.command {
    Some(existing) if existing != command => Err(ExecutorError::ExecutionIdentityError(format!(
      "execution was prepared for command '{existing}', not '{command}'"
    ))),
    _ => Ok(()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn lifecycle_is_idempotent_for_one_command_and_terminal_status() {
    let run = ExecutionRun::new(Arc::new(Console::default()), 7);

    let started_at = run.start("build").await.unwrap();
    assert_eq!(run.start("build").await.unwrap(), started_at);
    let finished_at = run.finish("build", ConsoleStatus::Success).await.unwrap();
    assert_eq!(run.finish("build", ConsoleStatus::Success).await.unwrap(), finished_at);
  }

  #[tokio::test]
  async fn lifecycle_rejects_command_and_status_changes() {
    let run = ExecutionRun::new(Arc::new(Console::default()), 7);

    run.start("build").await.unwrap();
    assert!(matches!(
      run.start("test").await,
      Err(ExecutorError::ExecutionIdentityError(_))
    ));
    run.finish("build", ConsoleStatus::Success).await.unwrap();
    assert!(matches!(
      run.finish("build", ConsoleStatus::Failed).await,
      Err(ExecutorError::ExecutionIdentityError(_))
    ));
  }
}
