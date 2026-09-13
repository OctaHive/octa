//! Remote HTTP adapter for Octa's action cache and content-addressed storage.
//!
//! This crate is intentionally an adapter, not a second cache orchestration
//! layer. It implements `octa_cache::CacheStore`; local-first policy and safe
//! workspace restoration remain in `octa-cache` and `octa-executor`.

#![warn(missing_docs)]

mod circuit;
mod client;
mod config;
mod transport;

use std::path::PathBuf;

pub use client::HttpCacheStore;
pub use config::{
  HttpCacheConfig, HttpCachePolicy, DEFAULT_CIRCUIT_FAILURE_THRESHOLD, DEFAULT_CIRCUIT_OPEN_DURATION,
  DEFAULT_MAX_PARALLEL_TRANSFERS, DEFAULT_MAX_RETRIES, DEFAULT_REQUEST_TIMEOUT, DEFAULT_RETRY_BASE_DELAY,
  DEFAULT_RETRY_MAX_DELAY, MAX_CIRCUIT_OPEN_DURATION, MAX_PARALLEL_TRANSFERS, MAX_REQUEST_TIMEOUT, MAX_RETRIES,
  MAX_RETRY_DELAY,
};

/// Failure while validating or constructing the HTTP adapter.
#[derive(Debug, thiserror::Error)]
pub enum HttpCacheError {
  /// Endpoint, credential, or operational policy is invalid.
  #[error("invalid HTTP cache configuration: {0}")]
  Configuration(String),
  /// A credential file could not be inspected without exposing its contents.
  #[error("failed to {operation} remote cache token file '{path}': {source}")]
  TokenFile {
    /// Stable filesystem operation.
    operation: &'static str,
    /// Credential path, never its contents.
    path: PathBuf,
    /// Underlying filesystem failure.
    #[source]
    source: std::io::Error,
  },
  /// A custom root-certificate file could not be read safely.
  #[error("failed to {operation} remote cache CA file '{path}': {source}")]
  CertificateFile {
    /// Stable filesystem operation.
    operation: &'static str,
    /// Public certificate path.
    path: PathBuf,
    /// Underlying filesystem failure.
    #[source]
    source: std::io::Error,
  },
  /// HTTP client construction or request building failed.
  #[error("HTTP cache client failed: {0}")]
  Request(#[from] reqwest::Error),
  /// Endpoint URL could not be extended with a protocol route.
  #[error("HTTP cache endpoint failed: {0}")]
  Url(#[from] url::ParseError),
  /// Protocol metadata is invalid.
  #[error(transparent)]
  Protocol(#[from] octa_cache_protocol::CacheProtocolError),
  /// Repeated failures opened the run-scoped circuit breaker.
  #[error("remote cache circuit is open for this run")]
  CircuitOpen,
}

impl From<HttpCacheError> for octa_cache::CacheError {
  fn from(error: HttpCacheError) -> Self {
    octa_cache::CacheError::Remote {
      operation: "client",
      message: error.to_string(),
    }
  }
}

#[cfg(test)]
mod tests;
