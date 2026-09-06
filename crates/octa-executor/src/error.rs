//! Typed errors produced while planning and executing tasks.
//!
//! These errors retain executor-specific context internally. Public terminal
//! results convert them into stable `ExecutionFailure` values at the API edge.

use std::time::SystemTimeError;

use glob::PatternError;
use octa_dag::error::DAGError;
use octa_octafile::OctafileError;
use octa_output::SourceLocation;
use thiserror::Error;
use tokio::task;

/// Result returned by executor operations before they reach the public terminal-result boundary.
pub type ExecutorResult<T> = Result<T, ExecutorError>;

/// Detailed planning and runtime failures retained inside the executor.
#[derive(Error, Debug)]
pub enum ExecutorError {
  /// Running tasks did not stop before the executor shutdown deadline.
  #[error("Shutdown timeout exceeded")]
  ShutdownTimeout,

  /// Cooperative cancellation stopped the named task.
  #[error("Task {0} cancelled")]
  TaskCancelled(String),

  /// A task exceeded its configured runtime limit.
  #[error("Task {task} exceeded timeout {timeout}")]
  TaskTimedOut {
    /// Task name shown to the user.
    task: String,
    /// Human-readable configured timeout.
    timeout: String,
  },

  /// A parsed task did not contain a usable plugin invocation.
  #[error("Missing plugin keys in task")]
  TaskParsedError,

  /// No running plugin provides the requested task type or capability.
  #[error("No running plugin provides task type '{0}'")]
  PluginUnavailable(String),

  /// Plugin input did not satisfy the plugin's validation schema.
  #[error("Invalid parameters for plugin '{0}': {1}")]
  PluginValidationFailed(String, String),

  /// The selected plugin cannot execute through a raw terminal session.
  #[error("Plugin '{0}' does not support raw/PTY execution")]
  RawUnsupported(String),

  /// Capturing both plugin streams would exceed the bounded result limit.
  #[error("Plugin '{plugin}' output exceeds the {limit_mib} MiB task-result limit")]
  PluginOutputTooLarge {
    /// Plugin whose output exceeded the limit.
    plugin: String,
    /// Configured result limit in mebibytes.
    limit_mib: usize,
  },

  /// A plugin-backed template or condition evaluation returned a failure status.
  #[error("Plugin '{key}' evaluation failed with status {code}: {stderr}")]
  PluginEvaluationFailed {
    /// Plugin key or capability used for evaluation.
    key: String,
    /// Process-style status returned by the plugin.
    code: i32,
    /// Captured diagnostic output.
    stderr: String,
    /// Octafile coordinates associated with the evaluation, when known.
    location: Option<SourceLocation>,
  },

  /// Runtime evaluation was requested without an available plugin evaluator.
  #[error("Plugin evaluation is unavailable while resolving '{0}'")]
  PluginEvaluationUnavailable(String),

  /// Task dependencies contain a cycle and cannot be scheduled.
  #[error("Cycle detected in task dependencies")]
  CycleDetected,

  /// The requested command does not exist in the selected task.
  #[error("Command not found: {0}")]
  CommandNotFound(String),

  /// A YAML value could not be decoded into the required executor shape.
  #[error("Failed to parse YAML Value: {0}")]
  DeserializeError(#[from] serde_yml::Error),

  /// The requested task was not present in the loaded project.
  #[error("Task not found: {0}")]
  TaskNotFound(String),

  /// A task node reported failure without a command exit status.
  #[error("Task execution failed: {0}")]
  TaskFailed(String),

  /// A Tera template could not be parsed.
  #[error("Template error: {0}")]
  TemplateParseFailed(String),

  /// A parsed template could not be rendered with the runtime context.
  #[error("Template render error: {0}")]
  TemplateRenderError(String),

  /// A task command completed with a non-zero status.
  #[error("Task {task} failed with exit code {code}: {stderr}")]
  CommandFailed {
    /// Task invocation containing the command.
    task: String,
    /// Non-zero exit status returned by the command.
    code: i32,
    /// Captured standard error used for diagnostics.
    stderr: String,
    /// Octafile coordinates associated with the command, when known.
    location: Option<SourceLocation>,
  },

  /// Run, task, or step identifiers violate one-execution ownership.
  #[error("Invalid execution identity: {0}")]
  ExecutionIdentityError(String),

  /// Persistent fingerprint storage could not be read or updated.
  #[error("Failed to get or set fingerprint db")]
  OpenFingerprintDbError(#[from] sled::Error),

  /// A freshness-dependent node ran before its shared decision was available.
  #[error("Freshness state is unavailable: {0}")]
  FreshnessStateUnavailable(String),

  /// A freshness decision was published more than once.
  #[error("Freshness state has already been published")]
  FreshnessStateAlreadyPublished,

  /// Inputs used as the persistent freshness identity could not be serialized.
  #[error("Failed to build freshness identity: {0}")]
  FreshnessIdentityError(String),

  /// No registered strategy can fingerprint the configured sources.
  #[error("No source strategy provider is registered for '{0}'")]
  SourceStrategyUnavailable(String),

  /// An internal task node was built without a required configuration value.
  #[error("Missing mandatory task configuration field: {0}")]
  TaskConfigFieldMissing(&'static str),

  /// A filesystem timestamp predates the supported epoch.
  #[error("Failed to calculate duration for time")]
  CalculateDurationError(#[from] SystemTimeError),

  /// A configured source or output glob is invalid.
  #[error("Failed to expand file pattern")]
  ExtendSourceError(#[from] PatternError),

  /// Loading or applying an `.octaignore` file failed.
  #[error("Failed to load .octaignore: {0}")]
  OctaignoreError(#[from] ignore::Error),

  /// A selected dotenv file could not be parsed or read.
  #[error("Failed to load environment file '{path}': {source}")]
  DotenvError {
    /// Path of the failing dotenv file.
    path: String,
    /// Underlying dotenv parser or I/O error.
    #[source]
    source: dotenvy::Error,
  },

  /// A dependency edge could not be inserted into the task graph.
  #[error("Failed to add graph dependency")]
  AddDependencyError(#[from] DAGError),

  /// Environment or task value expansion failed.
  #[error("Failed to expand value: {0}: {1}")]
  ValueExpandError(String, String),

  /// An internal scheduler channel closed before execution completed.
  #[error("Channel communication error")]
  ChannelError,

  /// The shared concurrency semaphore was closed.
  #[error("Concurrency limiter was closed")]
  ConcurrencyLimiterClosed,

  /// A filesystem, console, or terminal operation failed.
  #[error("IO error: {0}")]
  IoError(#[from] std::io::Error),

  /// Additional YAML configuration could not be represented as JSON.
  #[error("Failed to convert {0} yaml Value to json Value: {1}")]
  ExtraValueConvertError(String, String),

  /// A spawned executor task panicked or was aborted unexpectedly.
  #[error("Task join error: {0}")]
  JoinError(#[from] task::JoinError),

  /// No working directory was available for a task that requires one.
  #[error("Missing working directory configuration")]
  MissingWorkDir,

  /// A configured variable failed template or shell expansion.
  #[error("Failed to expand variable: {0}: {1}")]
  VariableExpandError(String, String),

  /// A required variable has no supplied, inherited, or prompted value.
  #[error("Required variable '{0}' is not set")]
  RequiredVariableMissing(String),

  /// A required value still contains a template when concrete input is required.
  #[error("Required variable '{0}' must be supplied as a concrete value")]
  RequiredVariableNotConcrete(String),

  /// A required value is outside its allowed enumeration.
  #[error("Required variable '{0}' must be one of: {1}")]
  RequiredVariableNotAllowed(String, String),

  /// The allowed values for a required variable could not be resolved.
  #[error("Failed to resolve enum for required variable '{0}': {1}")]
  RequiredVariableEnumError(String, String),

  /// Required input was requested in a headless execution without a resolver.
  #[error("Interactive input is unavailable for required variable '{0}'; supply {0}=VALUE on the command line")]
  VariablePromptUnavailable(String),

  /// The configured input resolver failed to obtain a required value.
  #[error("Failed to read required variable '{0}': {1}")]
  VariablePromptFailed(String, String),

  /// Loading a referenced or included Octafile failed.
  #[error("Failed to get included octafile: {0}")]
  GetCotafile(#[from] OctafileError),

  /// A shared runtime resource could not be locked.
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
        location: None,
      }
      .command_exit_code(),
      Some(17)
    );
    assert_eq!(
      ExecutorError::PluginEvaluationFailed {
        key: "shell".to_owned(),
        code: 0,
        stderr: String::new(),
        location: None,
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
