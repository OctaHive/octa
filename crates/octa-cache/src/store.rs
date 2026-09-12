//! Transport-independent cache storage boundary.
//!
//! Both the local CAS and the later remote HTTP client implement this one
//! interface. Filesystem discovery, bundle encoding, restoration, and garbage
//! collection remain concrete `octa-cache` behavior rather than becoming
//! provider APIs.

use std::pin::Pin;

use async_trait::async_trait;
use octa_cache_protocol::{ActionResultV1, BlobDescriptor, CacheLayer, Digest};
use tokio::io::AsyncRead;

use crate::CacheResult;

/// Owned asynchronous stream returned by a cache store.
pub type BlobReader = Pin<Box<dyn AsyncRead + Send + Unpin + 'static>>;

/// Outcome of an immutable create-if-absent publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
  /// This caller made the object visible.
  Written,
  /// An identical semantic object was already visible.
  AlreadyPresent,
  /// The action key was already bound to a different result.
  Conflict,
}

/// Verified action result together with the tier that supplied it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionLookup {
  /// Immutable action metadata selected by the store.
  pub result: ActionResultV1,
  /// Tier that supplied `result`.
  pub layer: CacheLayer,
}

/// Validates the namespace shared by local and future remote stores.
///
/// Stores may hash the value for their physical layout, but all implementations
/// accept the same bounded logical namespace.
pub fn validate_namespace(namespace: &str) -> CacheResult<()> {
  octa_cache_protocol::validate_namespace(namespace)?;
  Ok(())
}

/// Minimal storage operations shared by local and remote task-result caches.
///
/// Implementations must not expose partially written objects. Blob publication
/// precedes action publication, and action records are immutable once visible.
#[async_trait]
pub trait CacheStore: Send + Sync {
  /// Looks up and validates the result bound to `action` in `namespace`.
  async fn get_action(&self, namespace: &str, action: &Digest) -> CacheResult<Option<ActionLookup>>;

  /// Returns, in request order, the subset whose exact physical representations are absent.
  async fn find_missing_blobs(&self, blobs: &[BlobDescriptor]) -> CacheResult<Vec<BlobDescriptor>>;

  /// Opens an encoded blob stream whose length matches `blob`.
  ///
  /// Bundle decoding performs the final expanded digest and entry validation.
  async fn read_blob(&self, blob: &BlobDescriptor) -> CacheResult<BlobReader>;

  /// Durably publishes an encoded blob without replacing an existing object.
  async fn write_blob_if_absent(&self, blob: &BlobDescriptor, body: BlobReader) -> CacheResult<WriteOutcome>;

  /// Durably binds an action to a result after all referenced blobs are visible.
  async fn write_action_if_absent(&self, namespace: &str, result: &ActionResultV1) -> CacheResult<WriteOutcome>;
}
