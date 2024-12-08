use octa_dag::error::DAGError;
use octa_octafile::error::OctafileError;
use thiserror::Error;
use tokio::task;

pub type ExecutorResult<T> = Result<T, ExecutorError>;

#[derive(Error, Debug)]
pub enum ExecutorError {
  #[error("Cycle detected in task dependencies")]
  CycleDetected,

  #[error("Command not found: {0}")]
  CommandNotFound(String),

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

  #[error("Failed to add graph dependency")]
  AddDependencyError(#[from] DAGError),

  #[error("Failed to interpolate value: {0}: {1}")]
  ValueInterpolateError(String, String),

  #[error("Channel communication error")]
  ChannelError,

  #[error("IO error: {0}")]
  IoError(#[from] std::io::Error),

  #[error("Task join error: {0}")]
  JoinError(#[from] task::JoinError),

  #[error("Missing working directory configuration")]
  MissingWorkDir,

  #[error("Failed to interpolate variable: {0}: {1}")]
  VariableInterpolateError(String, String),

  #[error("Failed to get included octafile: {0}")]
  GetCotafile(#[from] OctafileError),

  #[error("Lock error: {0}")]
  LockError(String),
}
