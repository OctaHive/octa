use std::time::SystemTimeError;

use glob::{GlobError, PatternError};
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

  #[error("Command execution failed: {0}")]
  CommandFailed(String),

  #[error("Failed to get or set fingerprint db")]
  OpenFingerprintDbError(#[from] sled::Error),

  #[error("Failed to calculate duration for time")]
  CalculateDurationError(#[from] SystemTimeError),

  #[error("Failed to extend source path")]
  ExtendSourceError(#[from] PatternError),

  #[error("Failed to extend source path")]
  GlobError(#[from] GlobError),

  #[error("Failed to add graph dependency")]
  AddDependencyError(#[from] DAGError),

  #[error("Failed to expand value: {0}: {1}")]
  ValueExpandError(String, String),

  #[error("Channel communication error")]
  ChannelError,

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

  #[error("Failed to get included octafile: {0}")]
  GetCotafile(#[from] OctafileError),

  #[error("Lock error: {0}")]
  LockError(String),
}
