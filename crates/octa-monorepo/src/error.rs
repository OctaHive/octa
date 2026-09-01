use octa_octafile::OctafileError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MonorepoError {
  #[error("failed to inspect monorepo path: {0}")]
  Io(#[from] std::io::Error),

  #[error(transparent)]
  Octafile(#[from] OctafileError),

  #[error("invalid monorepo configuration: {0}")]
  InvalidConfiguration(String),

  #[error("invalid monorepo pattern '{pattern}': {message}")]
  InvalidPattern { pattern: String, message: String },

  #[error("failed to traverse monorepo: {0}")]
  Walk(#[from] ignore::Error),

  #[error("failed to access monorepo discovery cache: {0}")]
  Cache(#[from] sled::Error),

  #[error("failed to encode monorepo discovery cache: {0}")]
  CacheEncoding(#[from] serde_json::Error),
}
