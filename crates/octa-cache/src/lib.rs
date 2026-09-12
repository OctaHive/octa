//! Filesystem encoding engine for task-result caching.
//!
//! Input snapshots and output bundles live together because they enforce the
//! same workspace-relative path and filesystem safety rules. The executor
//! supplies task semantics; this crate never parses an Octafile and never
//! knows about CLI or runner transports.

#![warn(missing_docs)]

mod bundle;
mod error;
mod fileset;
mod hash;
mod input;
mod local;
mod locking;
mod platform;
mod restore;
mod store;
mod workspace;

pub use bundle::{
  extract_bundle, inspect_outputs, pack_bundle, BundleEncoding, BundleLimits, PackedBundle,
  DEFAULT_BUNDLE_COMPRESSION_LEVEL,
};
pub use error::{CacheError, CacheResult};
pub use input::{InputEntry, InputSnapshot, InputSnapshotter, SnapshotOptions};
pub use local::{GarbageCollection, LocalCacheConfig, LocalCacheStatus, LocalCacheStore};
pub use restore::{OutputLockGuard, RestoreManager, RestoreOutcome};
pub use store::{validate_namespace, ActionLookup, BlobReader, CacheStore, WriteOutcome};

/// Validates the shared cache/watch input grammar and non-overlapping output roots.
pub fn validate_file_contract(
  workspace: &std::path::Path,
  inputs: &[String],
  outputs: &[octa_cache_protocol::RelativePath],
) -> CacheResult<()> {
  if outputs.is_empty() {
    fileset::validate_contract(inputs, workspace, outputs)
  } else {
    let outputs = bundle::validate_output_roots(outputs)?;
    fileset::validate_contract(inputs, workspace, &outputs)
  }
}
