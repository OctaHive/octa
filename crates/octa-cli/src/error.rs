use dotenvy::Error as DotenvError;
use thiserror::Error;

use octa_executor::{ExecutionFailure, ExecutorError};
use octa_monorepo::MonorepoError;
use octa_octafile::OctafileError;
use octa_plugin_manager::plugin_lock::PluginLockError;
use octa_runtime::{RuntimeCacheError, RuntimeError};

pub type OctaResult<T> = Result<T, OctaError>;

#[derive(Error, Debug)]
pub enum OctaError {
  #[error(transparent)]
  Io(#[from] std::io::Error),

  #[error("Failed to execute task: {0}")]
  Runtime(String),

  #[error("Failed to start plugin: {0}")]
  PluginStartError(String),

  #[error("Failed to load environment file '{path}': {source}")]
  Dotenv {
    path: String,
    #[source]
    source: DotenvError,
  },

  #[error("Failed to load config file: {0}")]
  ConfigLoadError(String),

  #[error("cache management requires --cache-profile PATH")]
  CacheProfileRequired,

  #[error("selected task graph has no cacheable task to explain")]
  CacheExplainUnavailable,

  #[error(transparent)]
  Cache(#[from] RuntimeCacheError),

  #[error(transparent)]
  PluginLock(Box<PluginLockError>),

  #[error("Invalid CLI variable: {0}")]
  InvalidVariable(String),

  #[error("Invalid output configuration: {0}")]
  InvalidOutputConfig(String),

  #[error("Watch mode requires at least one task with files.inputs")]
  WatchSourcesMissing,

  #[error(transparent)]
  OctafileLoad(#[from] OctafileError),

  #[error(transparent)]
  Monorepo(#[from] MonorepoError),

  #[error(transparent)]
  ExecutionError(#[from] ExecutorError),

  #[error(transparent)]
  ExecutionFailed(#[from] Box<ExecutionFailure>),

  #[error(transparent)]
  RuntimeExecution(#[from] RuntimeError),
}

impl From<PluginLockError> for OctaError {
  fn from(error: PluginLockError) -> Self {
    Self::PluginLock(Box::new(error))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn preserves_plugin_lock_errors() {
    let error = OctaError::from(PluginLockError::MissingPlugin("shell".to_owned()));
    assert!(matches!(error, OctaError::PluginLock(_)));
    assert!(error.to_string().contains("shell"));
  }
}
