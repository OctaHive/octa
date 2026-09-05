use std::time::SystemTimeError;

use glob::PatternError;
use octa_dag::error::DAGError;
use octa_octafile::OctafileError;
use thiserror::Error;
use tokio::task;

pub type ExecutorResult<T> = Result<T, ExecutorError>;

#[derive(Error, Debug)]
pub enum ExecutorError {
  #[error("Shutdown timeout exceeded")]
  ShutdownTimeout,

  #[error("Task {0} cancelled")]
  TaskCancelled(String),

  #[error("Task {task} exceeded timeout {timeout}")]
  TaskTimedOut { task: String, timeout: String },

  #[error("Missing plugin keys in task")]
  TaskParsedError,

  #[error("No running plugin provides task type '{0}'")]
  PluginUnavailable(String),

  #[error("Invalid parameters for plugin '{0}': {1}")]
  PluginValidationFailed(String, String),

  #[error("Plugin '{0}' does not support raw/PTY execution")]
  RawUnsupported(String),

  #[error("Plugin '{plugin}' output exceeds the {limit_mib} MiB task-result limit")]
  PluginOutputTooLarge { plugin: String, limit_mib: usize },

  #[error("Plugin '{key}' evaluation failed with status {code}: {stderr}")]
  PluginEvaluationFailed { key: String, code: i32, stderr: String },

  #[error("Plugin evaluation is unavailable while resolving '{0}'")]
  PluginEvaluationUnavailable(String),

  #[error("Cycle detected in task dependencies")]
  CycleDetected,

  #[error("Command not found: {0}")]
  CommandNotFound(String),

  #[error("Failed to parse YAML Value: {0}")]
  DeserializeError(#[from] serde_yml::Error),

  #[error("Task not found: {0}")]
  TaskNotFound(String),

  #[error("Task execution failed: {0}")]
  TaskFailed(String),

  #[error("Template error: {0}")]
  TemplateParseFailed(String),

  #[error("Template render error: {0}")]
  TemplateRenderError(String),

  #[error("Task {task} failed with exit code {code}: {stderr}")]
  CommandFailed { task: String, code: i32, stderr: String },

  #[error("Failed to get or set fingerprint db")]
  OpenFingerprintDbError(#[from] sled::Error),

  #[error("Freshness state is unavailable: {0}")]
  FreshnessStateUnavailable(String),

  #[error("Freshness state has already been published")]
  FreshnessStateAlreadyPublished,

  #[error("Failed to build freshness identity: {0}")]
  FreshnessIdentityError(String),

  #[error("No source strategy provider is registered for '{0}'")]
  SourceStrategyUnavailable(String),

  #[error("Missing mandatory task configuration field: {0}")]
  TaskConfigFieldMissing(&'static str),

  #[error("Failed to calculate duration for time")]
  CalculateDurationError(#[from] SystemTimeError),

  #[error("Failed to expand file pattern")]
  ExtendSourceError(#[from] PatternError),

  #[error("Failed to load .octaignore: {0}")]
  OctaignoreError(#[from] ignore::Error),

  #[error("Failed to load environment file '{path}': {source}")]
  DotenvError {
    path: String,
    #[source]
    source: dotenvy::Error,
  },

  #[error("Failed to add graph dependency")]
  AddDependencyError(#[from] DAGError),

  #[error("Failed to expand value: {0}: {1}")]
  ValueExpandError(String, String),

  #[error("Channel communication error")]
  ChannelError,

  #[error("Concurrency limiter was closed")]
  ConcurrencyLimiterClosed,

  #[error("IO error: {0}")]
  IoError(#[from] std::io::Error),

  #[error("Failed to convert {0} yaml Value to json Value: {1}")]
  ExtraValueConvertError(String, String),

  #[error("Task join error: {0}")]
  JoinError(#[from] task::JoinError),

  #[error("Missing working directory configuration")]
  MissingWorkDir,

  #[error("Failed to expand variable: {0}: {1}")]
  VariableExpandError(String, String),

  #[error("Required variable '{0}' is not set")]
  RequiredVariableMissing(String),

  #[error("Required variable '{0}' must be supplied as a concrete value")]
  RequiredVariableNotConcrete(String),

  #[error("Required variable '{0}' must be one of: {1}")]
  RequiredVariableNotAllowed(String, String),

  #[error("Failed to resolve enum for required variable '{0}': {1}")]
  RequiredVariableEnumError(String, String),

  #[error("Interactive input is unavailable for required variable '{0}'; supply {0}=VALUE on the command line")]
  VariablePromptUnavailable(String),

  #[error("Failed to read required variable '{0}': {1}")]
  VariablePromptFailed(String, String),

  #[error("Failed to get included octafile: {0}")]
  GetCotafile(#[from] OctafileError),

  #[error("Lock error: {0}")]
  LockError(String),
}

impl ExecutorError {
  /// Returns the process exit code when this error originated from a completed command.
  pub(crate) fn command_exit_code(&self) -> Option<i32> {
    match self {
      Self::CommandFailed { code, .. } | Self::PluginEvaluationFailed { code, .. } if *code != 0 => Some(*code),
      _ => None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn exposes_only_nonzero_process_exit_codes() {
    assert_eq!(
      ExecutorError::CommandFailed {
        task: "build".to_owned(),
        code: 17,
        stderr: String::new(),
      }
      .command_exit_code(),
      Some(17)
    );
    assert_eq!(
      ExecutorError::PluginEvaluationFailed {
        key: "shell".to_owned(),
        code: 0,
        stderr: String::new(),
      }
      .command_exit_code(),
      None
    );
    assert_eq!(
      ExecutorError::TaskCancelled("build".to_owned()).command_exit_code(),
      None
    );
  }
}
