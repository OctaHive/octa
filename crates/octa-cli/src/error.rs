use dotenvy::Error as DotenvError;
use thiserror::Error;

use octa_executor::error::ExecutorError;
use octa_octafile::OctafileError;

pub type OctaResult<T> = Result<T, OctaError>;

#[derive(Error, Debug)]
pub enum OctaError {
  #[error("Failed to execute task: {0}")]
  Runtime(String),

  #[error("Failed to load .env file")]
  Dotenv(#[from] DotenvError),

  #[error("Failed to load config file: {0}")]
  ConfigLoadError(String),

  #[error(transparent)]
  OctafileLoad(#[from] OctafileError),

  #[error(transparent)]
  ExecutionError(#[from] ExecutorError),

  #[error("Failed to open fingerprint db")]
  OpenFingerprintDbError(#[from] sled::Error),
}
