//! Stable, serializable terminal results for runs, tasks, and steps.
//!
//! These types deliberately do not expose scheduler or plugin implementation
//! errors. Embedders can persist them without reconstructing state from the
//! event stream.

use std::{error::Error, fmt};

use chrono::{DateTime, Utc};
use octa_output::{ConsoleStatus, SourceLocation};
use serde::{Deserialize, Serialize};

use crate::{error::ExecutorError, task::ExecutionBinding};

/// Stable category of a failure produced while executing a run, task, or step.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExecutionFailureKind {
  /// Execution was stopped through cooperative cancellation.
  Cancelled,
  /// A configured execution deadline expired.
  Timeout,
  /// A command or plugin evaluation returned a non-zero status.
  Command,
  /// A task failed outside a concrete command invocation.
  Task,
  /// Plugin discovery, validation, or transport failed.
  Plugin,
  /// The requested execution could not be planned from its configuration.
  Configuration,
  /// Executor infrastructure failed independently of task configuration.
  Infrastructure,
}

/// Public, serializable failure detached from executor implementation details.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ExecutionFailure {
  /// Machine-readable failure category.
  pub kind: ExecutionFailureKind,
  /// Human-readable failure description.
  pub message: String,
  /// Task invocation that failed, when the failure originated in a task.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub task_id: Option<u64>,
  /// Executable step that failed, when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub step_id: Option<u64>,
  /// Non-zero process exit code, when the failure came from a completed command.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub exit_code: Option<i32>,
  /// Source coordinates reported for the failure, when available.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub location: Option<SourceLocation>,
}

impl ExecutionFailure {
  pub(crate) fn from_error(error: &ExecutorError, binding: Option<&ExecutionBinding>) -> Self {
    let kind = match error {
      ExecutorError::TaskCancelled(_) => ExecutionFailureKind::Cancelled,
      ExecutorError::TaskTimedOut { .. } => ExecutionFailureKind::Timeout,
      ExecutorError::CommandFailed { .. } | ExecutorError::PluginEvaluationFailed { .. } => {
        ExecutionFailureKind::Command
      },
      ExecutorError::TaskFailed(_) => ExecutionFailureKind::Task,
      ExecutorError::PluginUnavailable(_)
      | ExecutorError::PluginValidationFailed(..)
      | ExecutorError::RawUnsupported(_)
      | ExecutorError::PluginOutputTooLarge { .. }
      | ExecutorError::PluginEvaluationUnavailable(_) => ExecutionFailureKind::Plugin,
      ExecutorError::TaskParsedError
      | ExecutorError::CycleDetected
      | ExecutorError::CommandNotFound(_)
      | ExecutorError::DeserializeError(_)
      | ExecutorError::TaskNotFound(_)
      | ExecutorError::TemplateParseFailed(_)
      | ExecutorError::TemplateRenderError(_)
      | ExecutorError::FreshnessStateUnavailable(_)
      | ExecutorError::FreshnessStateAlreadyPublished
      | ExecutorError::FreshnessIdentityError(_)
      | ExecutorError::SourceStrategyUnavailable(_)
      | ExecutorError::TaskConfigFieldMissing(_)
      | ExecutorError::DotenvError { .. }
      | ExecutorError::OctaignoreError(_)
      | ExecutorError::ValueExpandError(..)
      | ExecutorError::ExtraValueConvertError(..)
      | ExecutorError::MissingWorkDir
      | ExecutorError::VariableExpandError(..)
      | ExecutorError::RequiredVariableMissing(_)
      | ExecutorError::RequiredVariableNotConcrete(_)
      | ExecutorError::RequiredVariableNotAllowed(..)
      | ExecutorError::RequiredVariableEnumError(..)
      | ExecutorError::VariablePromptUnavailable(_)
      | ExecutorError::VariablePromptFailed(..)
      | ExecutorError::ExecutionIdentityError(_)
      | ExecutorError::GetCotafile(_) => ExecutionFailureKind::Configuration,
      ExecutorError::ShutdownTimeout
      | ExecutorError::OpenFingerprintDbError(_)
      | ExecutorError::CalculateDurationError(_)
      | ExecutorError::ExtendSourceError(_)
      | ExecutorError::AddDependencyError(_)
      | ExecutorError::ChannelError
      | ExecutorError::ConcurrencyLimiterClosed
      | ExecutorError::IoError(_)
      | ExecutorError::JoinError(_)
      | ExecutorError::LockError(_) => ExecutionFailureKind::Infrastructure,
    };
    let message = match error {
      ExecutorError::CommandFailed { task, code, .. } => {
        format!("Task {task} failed with exit code {code}")
      },
      ExecutorError::PluginEvaluationFailed { key, code, .. } => {
        format!("Plugin '{key}' evaluation failed with status {code}")
      },
      _ => error.to_string(),
    };
    let location = match error {
      ExecutorError::CommandFailed { location, .. } | ExecutorError::PluginEvaluationFailed { location, .. } => {
        location.clone()
      },
      _ => None,
    };
    Self {
      kind,
      message,
      task_id: binding.map(|binding| binding.scope().id()),
      step_id: binding.and_then(ExecutionBinding::step).map(|step| step.id()),
      exit_code: error.command_exit_code(),
      location,
    }
  }

  pub(crate) fn synthetic(
    kind: ExecutionFailureKind,
    message: impl Into<String>,
    task_id: Option<u64>,
    step_id: Option<u64>,
  ) -> Self {
    Self {
      kind,
      message: message.into(),
      task_id,
      step_id,
      exit_code: None,
      location: None,
    }
  }
}

/// Role played by a task invocation in its owning run.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TaskRole {
  /// Work requested by the run or reached through normal task dependencies.
  #[default]
  Main,
  /// Cleanup work registered by a `defer` command; its failure does not replace the main result.
  Deferred,
}

impl fmt::Display for ExecutionFailure {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl Error for ExecutionFailure {}

/// Terminal state of a run, task, or step. Failure data cannot disagree with the state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "failure", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExecutionConclusion {
  /// Work completed successfully.
  Succeeded,
  /// Work intentionally performed no action.
  Skipped,
  /// Work ended with the attached failure.
  Failed(ExecutionFailure),
  /// Work was cancelled for the reason attached to the conclusion.
  Cancelled(ExecutionFailure),
}

impl ExecutionConclusion {
  /// Returns the event-layer status represented by this conclusion.
  pub fn status(&self) -> ConsoleStatus {
    match self {
      Self::Succeeded => ConsoleStatus::Success,
      Self::Skipped => ConsoleStatus::Skipped,
      Self::Failed(_) => ConsoleStatus::Failed,
      Self::Cancelled(_) => ConsoleStatus::Cancelled,
    }
  }

  /// Returns failure details for failed and cancelled conclusions.
  pub fn failure(&self) -> Option<&ExecutionFailure> {
    match self {
      Self::Failed(failure) | Self::Cancelled(failure) => Some(failure),
      Self::Succeeded | Self::Skipped => None,
    }
  }

  /// Returns whether the conclusion permits dependent work to proceed.
  pub fn is_success(&self) -> bool {
    matches!(self, Self::Succeeded | Self::Skipped)
  }
}

/// Identifies the ordered output events belonging to one task or step.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct OutputReference {
  /// Run containing the referenced output.
  pub run_id: u64,
  /// Task invocation containing the referenced output.
  pub task_id: u64,
  /// Optional step restriction; `None` selects every output event for the task.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub step_id: Option<u64>,
}

impl OutputReference {
  pub(crate) fn task(run_id: u64, task_id: u64) -> Self {
    Self {
      run_id,
      task_id,
      step_id: None,
    }
  }

  pub(crate) fn step(run_id: u64, task_id: u64, step_id: u64) -> Self {
    Self {
      run_id,
      task_id,
      step_id: Some(step_id),
    }
  }
}

/// Terminal result of one executable step.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StepResult {
  /// Execution-local step identifier.
  pub step_id: u64,
  /// Human-readable step label.
  pub label: String,
  /// Time the step started; absent when it reached a terminal state before scheduling.
  pub started_at: Option<DateTime<Utc>>,
  /// Time the terminal step event was accepted.
  pub finished_at: DateTime<Utc>,
  /// Terminal state and optional failure.
  pub conclusion: ExecutionConclusion,
  /// Selector for this step's streamed output events.
  pub output: OutputReference,
}

/// Terminal result of one task invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct TaskResult {
  /// Execution-local task invocation identifier.
  pub task_id: u64,
  /// Calling task invocation, when this task was nested.
  pub parent_task_id: Option<u64>,
  /// Human-readable invocation label.
  pub label: String,
  /// Whether this invocation belongs to normal execution or deferred cleanup.
  #[serde(default)]
  pub role: TaskRole,
  /// Time the invocation started; absent when it finished before scheduling.
  pub started_at: Option<DateTime<Utc>>,
  /// Time the terminal task event was accepted.
  pub finished_at: DateTime<Utc>,
  /// Terminal state and optional failure.
  pub conclusion: ExecutionConclusion,
  /// Selector for every streamed output event belonging to this task.
  pub output: OutputReference,
  /// Executable steps in declaration order.
  pub steps: Vec<StepResult>,
}

/// Complete structured outcome of one requested execution.
///
/// `tasks` is empty when cancellation or configuration failure ends preparation before a plan is
/// available to the scheduler.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ExecutionResult {
  /// Identifier shared with every event emitted by this run.
  pub run_id: u64,
  /// Requested command.
  pub command: String,
  /// Time execution entered the public API.
  pub started_at: DateTime<Utc>,
  /// Time the complete terminal snapshot was formed.
  pub finished_at: DateTime<Utc>,
  /// Overall terminal state and optional failure.
  pub conclusion: ExecutionConclusion,
  /// Task invocations ordered by their execution-local declaration IDs.
  pub tasks: Vec<TaskResult>,
  /// Values exported by successful DAG nodes for dependency interpolation.
  pub outputs: Vec<String>,
}

impl ExecutionResult {
  /// Returns the failure attached to a failed or cancelled run.
  pub fn failure(&self) -> Option<&ExecutionFailure> {
    self.conclusion.failure()
  }

  /// Returns whether the run succeeded or was skipped.
  pub fn is_success(&self) -> bool {
    self.conclusion.is_success()
  }

  /// Consumes the result and returns its failure, if any.
  pub fn into_failure(self) -> Option<ExecutionFailure> {
    match self.conclusion {
      ExecutionConclusion::Failed(failure) | ExecutionConclusion::Cancelled(failure) => Some(failure),
      ExecutionConclusion::Succeeded | ExecutionConclusion::Skipped => None,
    }
  }
}

pub(crate) fn conclusion(
  status: ConsoleStatus,
  failure: Option<ExecutionFailure>,
  task_id: Option<u64>,
  step_id: Option<u64>,
) -> ExecutionConclusion {
  match status {
    ConsoleStatus::Success => ExecutionConclusion::Succeeded,
    ConsoleStatus::Skipped => ExecutionConclusion::Skipped,
    ConsoleStatus::Failed => ExecutionConclusion::Failed(failure.unwrap_or_else(|| {
      ExecutionFailure::synthetic(ExecutionFailureKind::Task, "execution failed", task_id, step_id)
    })),
    ConsoleStatus::Cancelled => ExecutionConclusion::Cancelled(failure.unwrap_or_else(|| {
      ExecutionFailure::synthetic(ExecutionFailureKind::Cancelled, "execution cancelled", task_id, step_id)
    })),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_output::ConsoleScopeAllocator;

  #[test]
  fn maps_executor_errors_to_stable_failure_data() {
    let allocator = ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let step = allocator.step(&scope, "shell");
    let binding = ExecutionBinding::for_step(scope.clone(), step.clone());

    let failure = ExecutionFailure::from_error(
      &ExecutorError::CommandFailed {
        task: "build".to_owned(),
        code: 17,
        stderr: "compiler failed".to_owned(),
        location: Some(SourceLocation {
          file: "src/main.rs".to_owned(),
          line: Some(4),
          column: Some(2),
        }),
      },
      Some(&binding),
    );

    assert_eq!(failure.kind, ExecutionFailureKind::Command);
    assert_eq!(failure.task_id, Some(scope.id()));
    assert_eq!(failure.location.as_ref().unwrap().line, Some(4));
    assert_eq!(failure.step_id, Some(step.id()));
    assert_eq!(failure.exit_code, Some(17));
    assert_eq!(failure.message, "Task build failed with exit code 17");
    assert!(!failure.message.contains("compiler failed"));
    assert_eq!(failure.to_string(), failure.message);

    for (error, expected) in [
      (
        ExecutorError::TaskCancelled("build".to_owned()),
        ExecutionFailureKind::Cancelled,
      ),
      (
        ExecutorError::TaskTimedOut {
          task: "build".to_owned(),
          timeout: "1s".to_owned(),
        },
        ExecutionFailureKind::Timeout,
      ),
      (
        ExecutorError::TaskFailed("build".to_owned()),
        ExecutionFailureKind::Task,
      ),
      (
        ExecutorError::PluginUnavailable("shell".to_owned()),
        ExecutionFailureKind::Plugin,
      ),
      (
        ExecutorError::CommandNotFound("build".to_owned()),
        ExecutionFailureKind::Configuration,
      ),
      (
        ExecutorError::ConcurrencyLimiterClosed,
        ExecutionFailureKind::Infrastructure,
      ),
    ] {
      let failure = ExecutionFailure::from_error(&error, None);
      assert_eq!(failure.kind, expected);
      assert_eq!(failure.task_id, None);
      assert_eq!(failure.step_id, None);
      assert_eq!(failure.exit_code, None);
    }
  }

  #[test]
  fn conclusions_keep_terminal_state_and_failure_consistent() {
    assert_eq!(
      conclusion(ConsoleStatus::Success, None, None, None),
      ExecutionConclusion::Succeeded
    );
    assert_eq!(
      conclusion(ConsoleStatus::Skipped, None, None, None),
      ExecutionConclusion::Skipped
    );

    let failed = conclusion(ConsoleStatus::Failed, None, Some(3), Some(4));
    assert_eq!(failed.status(), ConsoleStatus::Failed);
    assert!(!failed.is_success());
    assert_eq!(failed.failure().unwrap().kind, ExecutionFailureKind::Task);
    assert_eq!(failed.failure().unwrap().task_id, Some(3));
    assert_eq!(failed.failure().unwrap().step_id, Some(4));

    let cancelled = conclusion(ConsoleStatus::Cancelled, None, Some(5), None);
    assert_eq!(cancelled.status(), ConsoleStatus::Cancelled);
    assert_eq!(cancelled.failure().unwrap().kind, ExecutionFailureKind::Cancelled);
  }

  #[test]
  fn execution_result_is_a_serializable_terminal_snapshot() {
    let now = Utc::now();
    let failure = ExecutionFailure {
      kind: ExecutionFailureKind::Command,
      message: "failed".to_owned(),
      task_id: Some(2),
      step_id: Some(7),
      exit_code: Some(9),
      location: Some(SourceLocation {
        file: "src/main.rs".to_owned(),
        line: Some(12),
        column: Some(3),
      }),
    };
    let result = ExecutionResult {
      run_id: 11,
      command: "build".to_owned(),
      started_at: now,
      finished_at: now,
      conclusion: ExecutionConclusion::Failed(failure.clone()),
      tasks: vec![TaskResult {
        task_id: 2,
        parent_task_id: Some(1),
        label: "compile".to_owned(),
        role: TaskRole::Deferred,
        started_at: Some(now),
        finished_at: now,
        conclusion: ExecutionConclusion::Failed(failure),
        output: OutputReference::task(11, 2),
        steps: vec![StepResult {
          step_id: 7,
          label: "shell".to_owned(),
          started_at: Some(now),
          finished_at: now,
          conclusion: ExecutionConclusion::Succeeded,
          output: OutputReference::step(11, 2, 7),
        }],
      }],
      outputs: vec!["artifact".to_owned()],
    };

    let encoded = serde_json::to_string(&result).unwrap();
    let decoded: ExecutionResult = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, result);
    assert!(!decoded.is_success());
    assert_eq!(decoded.failure().unwrap().location.as_ref().unwrap().line, Some(12));
    assert_eq!(decoded.into_failure().unwrap().exit_code, Some(9));
  }
}
