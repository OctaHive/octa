//! Stable cache identities and serialization-only result types.
//!
//! This crate is the lowest cache boundary shared by the CLI, runner, local
//! store, and future OctaCity HTTP client. It deliberately has no executor,
//! filesystem, async-runtime, compression, or transport dependency. Action
//! digests are produced by an explicit binary encoding rather than a serde
//! representation, so harmless JSON formatting changes cannot invalidate the
//! cache.

#![warn(missing_docs)]

mod action;
mod digest;
mod layer;
mod path;
mod policy;
mod result;

pub use action::{
  ActionDescriptorV1, PlatformArchitecture, PlatformOs, PluginIdentity, RuntimeIdentity, ACTION_KEY_FORMAT_V1,
};
pub use digest::{Digest, DigestAlgorithm};
pub use layer::CacheLayer;
pub use path::RelativePath;
pub use policy::CacheMode;
pub use result::{
  ActionResultV1, BlobDescriptor, BlobEncoding, CachedArtifact, CachedReport, ACTION_RESULT_VERSION_V1,
  ZSTD_V1_MAX_WINDOW_LOG,
};

/// JSON schema for serialized version-one action descriptors.
pub const ACTION_DESCRIPTOR_SCHEMA_V1: &str = include_str!("../schema/action-descriptor-v1.schema.json");
/// JSON schema for serialized version-one action results.
pub const ACTION_RESULT_SCHEMA_V1: &str = include_str!("../schema/action-result-v1.schema.json");

/// Maximum UTF-8 bytes in one identity or resource string.
pub const MAX_CACHE_STRING_BYTES: usize = 16 * 1024;
/// Maximum plugins, arguments, artifacts, or reports in one protocol value.
pub const MAX_CACHE_LIST_ITEMS: usize = 4096;
/// Maximum serialized public task output and bounded stdout metadata.
pub const MAX_ACTION_RESULT_METADATA_BYTES: usize = 4 * 1024 * 1024;
/// Maximum complete JSON representation accepted for one action result.
pub const MAX_ACTION_RESULT_WIRE_BYTES: usize = MAX_ACTION_RESULT_METADATA_BYTES + 64 * 1024;

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
/// Invalid cache identity or result metadata.
pub enum CacheProtocolError {
  /// A digest string or size is malformed.
  #[error("invalid digest: {0}")]
  Digest(String),
  /// A workspace-relative path is not portable or normalized.
  #[error("invalid relative path: {0}")]
  Path(String),
  /// An action descriptor violates a version-one bound or invariant.
  #[error("invalid action descriptor: {0}")]
  Action(String),
  /// An action result violates a version-one bound or invariant.
  #[error("invalid action result: {0}")]
  Result(String),
  /// A shared cache-session value is malformed.
  #[error("invalid cache configuration: {0}")]
  Configuration(String),
}

/// Validates the logical namespace shared by every cache transport.
///
/// Keeping this rule in the wire crate prevents the runner, local store, and
/// future HTTP client from accepting subtly different authorization scopes.
pub fn validate_namespace(namespace: &str) -> Result<(), CacheProtocolError> {
  if namespace.is_empty() || namespace.len() > MAX_CACHE_STRING_BYTES || namespace.chars().any(char::is_control) {
    return Err(CacheProtocolError::Configuration(format!(
      "namespace must contain 1 to {MAX_CACHE_STRING_BYTES} UTF-8 bytes and no control characters"
    )));
  }
  Ok(())
}

pub(crate) fn validate_string(kind: &str, value: &str) -> Result<(), CacheProtocolError> {
  if value.is_empty() {
    return Err(CacheProtocolError::Action(format!("{kind} must not be empty")));
  }
  if value.len() > MAX_CACHE_STRING_BYTES {
    return Err(CacheProtocolError::Action(format!(
      "{kind} exceeds {MAX_CACHE_STRING_BYTES} UTF-8 bytes"
    )));
  }
  if value.chars().any(char::is_control) {
    return Err(CacheProtocolError::Action(format!(
      "{kind} must not contain control characters"
    )));
  }
  Ok(())
}

#[cfg(test)]
mod tests;
