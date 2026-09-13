//! Version-one request envelopes for the remote action cache and blob store.
//!
//! Blob bytes are deliberately not represented here: the HTTP transport streams
//! them separately. These bounded DTOs cover only metadata that must be decoded
//! before a transfer starts.

use serde::{Deserialize, Serialize};

use crate::{validate_namespace, ActionResultV1, BlobDescriptor, CacheProtocolError, MAX_CACHE_LIST_ITEMS};

/// Remote cache protocol version carried by every request and response.
pub const REMOTE_CACHE_PROTOCOL_V1: u16 = 1;
/// HTTP header carrying [`REMOTE_CACHE_PROTOCOL_V1`] on every request and response.
pub const REMOTE_CACHE_PROTOCOL_HEADER: &str = "x-octa-cache-protocol";
/// Canonical HTTP text for [`REMOTE_CACHE_PROTOCOL_V1`].
pub const REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1: &str = "1";
/// Media type for bounded version-one metadata envelopes.
pub const REMOTE_CACHE_JSON_CONTENT_TYPE: &str = "application/vnd.octa.cache.v1+json";
/// Media type for an encoded immutable blob representation.
pub const REMOTE_CACHE_BLOB_CONTENT_TYPE: &str = "application/vnd.octa.cache.blob";

/// Batched CAS-presence request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FindMissingBlobsRequestV1 {
  /// Protocol version; must be [`REMOTE_CACHE_PROTOCOL_V1`].
  pub protocol_version: u16,
  /// Exact physical blob representations whose presence is queried.
  pub blobs: Vec<BlobDescriptor>,
}

impl FindMissingBlobsRequestV1 {
  /// Validates the version, batch bound, and every descriptor.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    validate_version(self.protocol_version)?;
    validate_blobs(&self.blobs)
  }
}

/// Batched CAS-presence response, retaining request-order semantics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FindMissingBlobsResponseV1 {
  /// Protocol version; must be [`REMOTE_CACHE_PROTOCOL_V1`].
  pub protocol_version: u16,
  /// Subset of requested descriptors that the service cannot currently read.
  pub missing: Vec<BlobDescriptor>,
}

impl FindMissingBlobsResponseV1 {
  /// Validates the version, batch bound, and every descriptor.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    validate_version(self.protocol_version)?;
    validate_blobs(&self.missing)
  }
}

/// Immutable action publication request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WriteActionRequestV1 {
  /// Protocol version; must be [`REMOTE_CACHE_PROTOCOL_V1`].
  pub protocol_version: u16,
  /// Server-authorized logical scope for the action mapping.
  pub namespace: String,
  /// Validated action result published with create-if-absent semantics.
  pub result: ActionResultV1,
}

impl WriteActionRequestV1 {
  /// Validates the envelope and embedded action result.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    validate_version(self.protocol_version)?;
    validate_namespace(&self.namespace)?;
    self.result.validate()
  }
}

fn validate_version(version: u16) -> Result<(), CacheProtocolError> {
  if version != REMOTE_CACHE_PROTOCOL_V1 {
    return Err(CacheProtocolError::Configuration(format!(
      "unsupported remote cache protocol version {version}"
    )));
  }
  Ok(())
}

fn validate_blobs(blobs: &[BlobDescriptor]) -> Result<(), CacheProtocolError> {
  if blobs.len() > MAX_CACHE_LIST_ITEMS {
    return Err(CacheProtocolError::Configuration(format!(
      "remote blob batches are limited to {MAX_CACHE_LIST_ITEMS} items"
    )));
  }
  for blob in blobs {
    blob.validate()?;
  }
  Ok(())
}
