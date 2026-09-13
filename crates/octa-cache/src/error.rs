//! Errors shared by cache filesystem operations.

use std::{io, path::PathBuf};

use octa_cache_protocol::CacheProtocolError;
use tokio_util::sync::CancellationToken;

/// Result type returned by cache filesystem operations.
pub type CacheResult<T> = Result<T, CacheError>;

/// Failure produced while discovering, hashing, packing, or extracting cache data.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
  /// The caller requested cancellation before the operation completed.
  #[error("cache operation was cancelled")]
  Cancelled,
  /// A caller-supplied cache option or collection of options is invalid.
  #[error("invalid cache configuration: {0}")]
  Configuration(String),
  /// A filesystem path is unsafe, non-portable, or outside its workspace.
  #[error("invalid cache path '{path}': {reason}")]
  Path {
    /// Path that failed validation.
    path: PathBuf,
    /// Human-readable validation failure.
    reason: String,
  },
  /// A declared input or output is not a regular file, directory, or symlink.
  #[error("unsupported filesystem entry '{path}': {kind}")]
  UnsupportedEntry {
    /// Unsupported filesystem entry.
    path: PathBuf,
    /// Supported-kind requirement that the entry violated.
    kind: &'static str,
  },
  /// A filesystem entry changed repeatedly while Octa was reading it.
  #[error("filesystem entry '{path}' changed while cache data was being read")]
  UnstableFile {
    /// Entry whose identity did not remain stable.
    path: PathBuf,
  },
  /// An output bundle violates the canonical format or declared descriptor.
  #[error("invalid output bundle: {0}")]
  InvalidBundle(String),
  /// An operation exceeded a configured resource or size bound.
  #[error("cache limit exceeded: {0}")]
  Limit(String),
  /// Reading the untrusted encoded or canonical bundle source failed.
  ///
  /// This provenance is intentionally distinct from destination I/O: a
  /// layered cache may retry this error from an independent tier, while a
  /// staging or workspace failure cannot be repaired by downloading again.
  #[error("cache bundle source failed: {0}")]
  BundleSource(#[source] io::Error),
  /// Writing the caller-provided bundle sink failed while packing or extracting.
  #[error("cache bundle stream failed: {0}")]
  Stream(#[from] io::Error),
  /// Creating, reopening, or synchronizing private staging storage failed.
  #[error("cache temporary file failed: {0}")]
  TemporaryFile(#[source] io::Error),
  /// A shared cache protocol value failed semantic validation.
  #[error("cache protocol value is invalid: {0}")]
  Protocol(#[from] CacheProtocolError),
  /// Stored metadata could not be serialized or decoded safely.
  #[error("invalid cache metadata: {0}")]
  Metadata(String),
  /// A remote cache operation failed before producing a trusted result.
  #[error("remote cache {operation} failed: {message}")]
  Remote {
    /// Stable operation name suitable for diagnostics.
    operation: &'static str,
    /// Credential-free diagnostic suitable for operator logs.
    message: String,
  },
  /// A cache object failed integrity checks and was removed from normal lookup.
  #[error("corrupt cache object '{path}': {reason}")]
  Corrupt {
    /// Object moved to quarantine.
    path: PathBuf,
    /// Integrity failure that made the object unusable.
    reason: String,
  },
  /// A filesystem operation failed for a particular path.
  #[error("failed to {operation} '{path}': {source}")]
  Io {
    /// Short description of the attempted filesystem operation.
    operation: &'static str,
    /// Filesystem path on which the operation was attempted.
    path: PathBuf,
    /// Underlying operating-system error.
    #[source]
    source: io::Error,
  },
  /// A blocking worker could not be started or completed unexpectedly.
  #[error("cache worker failed: {0}")]
  Worker(#[source] tokio::task::JoinError),
  /// A shared worker disappeared after its error crossed a cloneable channel.
  #[error("cache worker failed: {0}")]
  WorkerTerminated(String),
  /// Releasing a pessimistic capacity reservation failed after another error.
  #[error("{primary}; additionally failed to release cache capacity: {release}")]
  CapacityRelease {
    /// Failure that prevented publication.
    primary: Box<CacheError>,
    /// Failure while correcting the capacity ledger.
    release: Box<CacheError>,
  },
}

pub(crate) fn io_error(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> CacheError {
  CacheError::Io {
    operation,
    path: path.into(),
    source,
  }
}

/// Applies the crate-wide cancellation error at synchronous work boundaries.
pub(crate) fn check_cancelled(cancel: &CancellationToken) -> CacheResult<()> {
  if cancel.is_cancelled() {
    Err(CacheError::Cancelled)
  } else {
    Ok(())
  }
}
