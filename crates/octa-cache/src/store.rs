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

/// Validated action-result metadata together with the tier that supplied it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionLookup {
  /// Immutable action metadata selected and structurally validated by the store.
  pub result: ActionResultV1,
  /// Tier that supplied `result`.
  pub layer: CacheLayer,
}

/// Validates the namespace shared by local and remote stores.
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

  /// Returns, in request order, the subset this store cannot currently read.
  ///
  /// A concrete CAS reports physical absence. A layered store reports a blob
  /// only when every readable tier lacks it; tier-specific publication checks
  /// remain an implementation detail of that layered store.
  async fn find_missing_blobs(&self, blobs: &[BlobDescriptor]) -> CacheResult<Vec<BlobDescriptor>>;

  /// Opens an encoded blob stream whose length matches `blob`.
  ///
  /// Bundle decoding performs the final expanded digest and entry validation.
  async fn read_blob(&self, blob: &BlobDescriptor) -> CacheResult<BlobReader>;

  /// Revalidates a suspect blob and may return an independently readable copy.
  ///
  /// A single-tier store returns a copy only when another writer repaired the
  /// object concurrently; otherwise it quarantines corruption and returns
  /// `None`. Layered stores can then fetch another tier without exposing their
  /// topology to the executor. Every returned stream remains untrusted and is
  /// validated through the same path as the original.
  async fn recover_corrupt_blob(&self, _blob: &BlobDescriptor) -> CacheResult<Option<BlobReader>> {
    Ok(None)
  }

  /// Durably publishes an encoded blob without replacing an existing object.
  async fn write_blob_if_absent(&self, blob: &BlobDescriptor, body: BlobReader) -> CacheResult<WriteOutcome>;

  /// Durably binds an action to a result after all referenced blobs are visible.
  async fn write_action_if_absent(&self, namespace: &str, result: &ActionResultV1) -> CacheResult<WriteOutcome>;

  /// Commits store-internal state after a hit was completely verified.
  ///
  /// Concrete stores normally have nothing to do because the hit already came
  /// from their durable state. A layered store uses this point to publish the
  /// verified remote action into its local hot tier without repeating remote
  /// publication. The executor calls this only after output restoration and
  /// resource validation have succeeded.
  async fn commit_verified_hit(&self, _namespace: &str, _result: &ActionResultV1) -> CacheResult<()> {
    Ok(())
  }
}
