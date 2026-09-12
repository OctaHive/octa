use octa_octafile::OctafileError;
use thiserror::Error;

/// Failure while resolving a monorepo or maintaining its discovery metadata.
#[derive(Debug, Error)]
pub enum MonorepoError {
  /// A filesystem path could not be inspected.
  #[error("failed to inspect monorepo path: {0}")]
  Io(#[from] std::io::Error),

  /// The root or project Octafile could not be inspected or parsed.
  #[error(transparent)]
  Octafile(#[from] OctafileError),

  /// The declared monorepo settings are internally inconsistent.
  #[error("invalid monorepo configuration: {0}")]
  InvalidConfiguration(String),

  /// A root or exclusion pattern is unsafe or syntactically invalid.
  #[error("invalid monorepo pattern '{pattern}': {message}")]
  InvalidPattern {
    /// Original pattern from the Octafile.
    pattern: String,
    /// Validation or glob-compilation failure.
    message: String,
  },

  /// Directory traversal failed while discovering project Octafiles.
  #[error("failed to traverse monorepo: {0}")]
  Walk(#[from] ignore::Error),

  /// The private persistent discovery database could not be accessed.
  #[error("failed to access monorepo discovery cache: {0}")]
  Cache(#[from] sled::Error),

  /// Discovery metadata could not be encoded for persistence.
  #[error("failed to encode monorepo discovery cache: {0}")]
  CacheEncoding(#[from] serde_json::Error),
}
