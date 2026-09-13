//! Local-first composition of the local CAS and one remote `CacheStore`.
//!
//! This module owns tier ordering only. It does not know HTTP, credentials, or
//! retry policy; those belong to the remote adapter. Remote lookup failures
//! remain distinguishable from real misses, while publication failures degrade
//! to local-only durability. Cancellation is never swallowed.

use std::sync::{
  atomic::{AtomicBool, Ordering},
  Arc,
};

use async_trait::async_trait;
use octa_cache_protocol::{ActionResultV1, BlobDescriptor, CacheLayer, Digest};

use crate::{ActionLookup, BlobReader, CacheError, CacheResult, CacheStore, LocalCacheStore, WriteOutcome};

/// A local L1 cache backed by an optional, independently resilient L2 store.
pub struct LayeredCacheStore {
  local: Arc<LocalCacheStore>,
  remote: Arc<dyn CacheStore>,
  remote_warning_emitted: AtomicBool,
}

impl std::fmt::Debug for LayeredCacheStore {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.debug_struct("LayeredCacheStore").finish_non_exhaustive()
  }
}

impl LayeredCacheStore {
  /// Composes one concrete local store with one transport adapter.
  pub fn new(local: Arc<LocalCacheStore>, remote: Arc<dyn CacheStore>) -> Self {
    Self {
      local,
      remote,
      remote_warning_emitted: AtomicBool::new(false),
    }
  }

  fn remote_warning(&self, operation: &'static str, error: &CacheError) {
    if !self.remote_warning_emitted.swap(true, Ordering::Relaxed) {
      tracing::warn!(cache.layer = "remote", cache.operation = operation, error = %error, "remote cache degraded");
    } else {
      tracing::debug!(cache.layer = "remote", cache.operation = operation, error = %error, "remote cache remains degraded");
    }
  }

  fn remote_success(&self) {
    self.remote_warning_emitted.store(false, Ordering::Relaxed);
  }
}

#[async_trait]
impl CacheStore for LayeredCacheStore {
  async fn get_action(&self, namespace: &str, action: &Digest) -> CacheResult<Option<ActionLookup>> {
    match self.local.get_action(namespace, action).await {
      Ok(Some(result)) => return Ok(Some(result)),
      Ok(None) => {},
      Err(CacheError::Cancelled) => return Err(CacheError::Cancelled),
      Err(error) => {
        tracing::warn!(cache.layer = "local", cache.operation = "lookup", error = %error, "local cache degraded")
      },
    }

    let lookup = match self.remote.get_action(namespace, action).await {
      Ok(result) => {
        self.remote_success();
        result
      },
      Err(CacheError::Cancelled) => return Err(CacheError::Cancelled),
      Err(error) => {
        self.remote_warning("lookup", &error);
        return Err(error);
      },
    };
    let Some(mut lookup) = lookup else {
      return Ok(None);
    };
    // Do not fetch the bundle here. The executor must validate the descriptor
    // against job limits before `read_blob` is allowed to consume disk space.
    lookup.layer = CacheLayer::Remote;
    Ok(Some(lookup))
  }

  async fn find_missing_blobs(&self, blobs: &[BlobDescriptor]) -> CacheResult<Vec<BlobDescriptor>> {
    let local_missing = match self.local.find_missing_blobs(blobs).await {
      Ok(missing) => missing,
      Err(CacheError::Cancelled) => return Err(CacheError::Cancelled),
      Err(error) => {
        tracing::warn!(cache.layer = "local", cache.operation = "find missing blobs", error = %error, "local cache degraded");
        blobs.to_vec()
      },
    };
    if local_missing.is_empty() {
      return Ok(Vec::new());
    }
    let remote_missing = match self.remote.find_missing_blobs(&local_missing).await {
      Ok(missing) => {
        self.remote_success();
        missing
      },
      Err(CacheError::Cancelled) => return Err(CacheError::Cancelled),
      Err(error) => {
        self.remote_warning("find missing blobs", &error);
        return Err(error);
      },
    };
    let remote_missing = remote_missing.into_iter().collect::<std::collections::BTreeSet<_>>();
    let requested = local_missing.iter().collect::<std::collections::BTreeSet<_>>();
    if remote_missing.len() > requested.len() || !remote_missing.iter().all(|blob| requested.contains(blob)) {
      return Err(CacheError::Metadata(
        "remote cache returned an invalid missing-blob response".to_owned(),
      ));
    }
    // A layered read misses only if neither tier can supply the representation.
    // Publication performs its L2-specific synchronization separately.
    Ok(
      local_missing
        .into_iter()
        .filter(|blob| remote_missing.contains(blob))
        .collect(),
    )
  }

  async fn read_blob(&self, blob: &BlobDescriptor) -> CacheResult<BlobReader> {
    match self.local.read_blob(blob).await {
      Ok(reader) => Ok(reader),
      Err(CacheError::Cancelled) => Err(CacheError::Cancelled),
      Err(local_error) => match self.remote.read_blob(blob).await {
        Ok(reader) => {
          self.remote_success();
          match self.local.write_blob_if_absent(blob, reader).await? {
            WriteOutcome::Written | WriteOutcome::AlreadyPresent => self.local.read_blob(blob).await,
            WriteOutcome::Conflict => Err(CacheError::Metadata(
              "local CAS rejected a remote blob with the same immutable identity".to_owned(),
            )),
          }
        },
        Err(CacheError::Cancelled) => Err(CacheError::Cancelled),
        Err(remote_error) => {
          self.remote_warning("read blob", &remote_error);
          tracing::debug!(cache.layer = "local", cache.operation = "read blob", error = %local_error, "local blob was unavailable before remote fallback");
          Err(remote_error)
        },
      },
    }
  }

  async fn write_blob_if_absent(&self, blob: &BlobDescriptor, body: BlobReader) -> CacheResult<WriteOutcome> {
    let local = self.local.write_blob_if_absent(blob, body).await?;
    if local == WriteOutcome::Conflict {
      return Ok(local);
    }
    let missing = match self.remote.find_missing_blobs(std::slice::from_ref(blob)).await {
      Ok(missing) if missing.is_empty() => {
        self.remote_success();
        return Ok(local);
      },
      Ok(missing) if missing.as_slice() == std::slice::from_ref(blob) => {
        self.remote_success();
        missing
      },
      Ok(_) => {
        let error = CacheError::Metadata("remote cache returned an invalid missing-blob response".to_owned());
        self.remote_warning("find missing blobs before publication", &error);
        return Ok(local);
      },
      Err(CacheError::Cancelled) => return Err(CacheError::Cancelled),
      Err(error) => {
        self.remote_warning("find missing blobs before publication", &error);
        return Ok(local);
      },
    };
    debug_assert_eq!(missing.as_slice(), std::slice::from_ref(blob));
    let reader = self.local.read_blob(blob).await?;
    match self.remote.write_blob_if_absent(blob, reader).await {
      Ok(WriteOutcome::Conflict) => {
        self.remote_success();
        Ok(WriteOutcome::Conflict)
      },
      Ok(_) => {
        self.remote_success();
        Ok(local)
      },
      Err(CacheError::Cancelled) => Err(CacheError::Cancelled),
      Err(error) => {
        self.remote_warning("write blob", &error);
        Ok(local)
      },
    }
  }

  async fn write_action_if_absent(&self, namespace: &str, result: &ActionResultV1) -> CacheResult<WriteOutcome> {
    // Detect an existing local nondeterministic result before publishing a new
    // mapping remotely. The final local create-if-absent still closes the race
    // with another writer in this process or on this machine.
    let local_existing = self.local.get_action(namespace, &result.action).await?;
    if let Some(existing) = &local_existing {
      if existing.result != *result {
        return Ok(WriteOutcome::Conflict);
      }
    }

    let remote = match self.remote.write_action_if_absent(namespace, result).await {
      Ok(WriteOutcome::Conflict) => {
        self.remote_success();
        return Ok(WriteOutcome::Conflict);
      },
      Ok(outcome) => {
        self.remote_success();
        Some(outcome)
      },
      Err(CacheError::Cancelled) => return Err(CacheError::Cancelled),
      Err(error) => {
        self.remote_warning("write action", &error);
        None
      },
    };

    let local = if local_existing.is_some() {
      WriteOutcome::AlreadyPresent
    } else {
      self.local.write_action_if_absent(namespace, result).await?
    };
    if local == WriteOutcome::Conflict {
      return Ok(local);
    }
    if local == WriteOutcome::Written || remote == Some(WriteOutcome::Written) {
      Ok(WriteOutcome::Written)
    } else {
      Ok(WriteOutcome::AlreadyPresent)
    }
  }

  async fn commit_verified_hit(&self, namespace: &str, result: &ActionResultV1) -> CacheResult<()> {
    match self.local.write_action_if_absent(namespace, result).await? {
      WriteOutcome::Written | WriteOutcome::AlreadyPresent => Ok(()),
      WriteOutcome::Conflict => {
        self.local.quarantine_conflicting_action(namespace, result).await?;
        match self.local.write_action_if_absent(namespace, result).await? {
          WriteOutcome::Written | WriteOutcome::AlreadyPresent => Ok(()),
          WriteOutcome::Conflict => Err(CacheError::Metadata(
            "local action changed again while promoting a verified remote hit".to_owned(),
          )),
        }
      },
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::LocalCacheConfig;
  use octa_cache_protocol::{BlobEncoding, ACTION_RESULT_VERSION_V1};
  use std::sync::atomic::AtomicUsize;
  use tempfile::TempDir;
  use tokio::io::AsyncReadExt as _;

  #[derive(Clone, Copy)]
  enum Behavior {
    Failure,
    Cancelled,
    Conflict,
    Readable,
    BlobConflict,
    InvalidMissing,
  }

  struct StubStore {
    behavior: Behavior,
    bytes: Vec<u8>,
  }

  struct HitStore {
    result: ActionResultV1,
    bytes: Vec<u8>,
    reads: AtomicUsize,
    missing_queries: AtomicUsize,
    blob_writes: AtomicUsize,
    action_writes: AtomicUsize,
  }

  #[async_trait]
  impl CacheStore for HitStore {
    async fn get_action(&self, _namespace: &str, _action: &Digest) -> CacheResult<Option<ActionLookup>> {
      Ok(Some(ActionLookup {
        result: self.result.clone(),
        layer: CacheLayer::Remote,
      }))
    }

    async fn find_missing_blobs(&self, _blobs: &[BlobDescriptor]) -> CacheResult<Vec<BlobDescriptor>> {
      self.missing_queries.fetch_add(1, Ordering::Relaxed);
      Ok(Vec::new())
    }

    async fn read_blob(&self, _blob: &BlobDescriptor) -> CacheResult<BlobReader> {
      self.reads.fetch_add(1, Ordering::Relaxed);
      Ok(Box::pin(std::io::Cursor::new(self.bytes.clone())))
    }

    async fn write_blob_if_absent(&self, _blob: &BlobDescriptor, _body: BlobReader) -> CacheResult<WriteOutcome> {
      self.blob_writes.fetch_add(1, Ordering::Relaxed);
      Ok(WriteOutcome::AlreadyPresent)
    }

    async fn write_action_if_absent(&self, _namespace: &str, _result: &ActionResultV1) -> CacheResult<WriteOutcome> {
      self.action_writes.fetch_add(1, Ordering::Relaxed);
      Ok(WriteOutcome::AlreadyPresent)
    }
  }

  #[async_trait]
  impl CacheStore for StubStore {
    async fn get_action(&self, _namespace: &str, _action: &Digest) -> CacheResult<Option<ActionLookup>> {
      match self.behavior {
        Behavior::Cancelled => Err(CacheError::Cancelled),
        Behavior::Failure => Err(remote_failure()),
        _ => Ok(None),
      }
    }

    async fn find_missing_blobs(&self, blobs: &[BlobDescriptor]) -> CacheResult<Vec<BlobDescriptor>> {
      match self.behavior {
        Behavior::Cancelled => Err(CacheError::Cancelled),
        Behavior::Failure => Err(remote_failure()),
        Behavior::Conflict => Ok(Vec::new()),
        Behavior::Readable | Behavior::BlobConflict => Ok(blobs.to_vec()),
        Behavior::InvalidMissing => Ok(blobs.iter().cloned().chain(blobs.iter().cloned()).collect()),
      }
    }

    async fn read_blob(&self, _blob: &BlobDescriptor) -> CacheResult<BlobReader> {
      match self.behavior {
        Behavior::Cancelled => Err(CacheError::Cancelled),
        Behavior::Failure | Behavior::Conflict | Behavior::BlobConflict | Behavior::InvalidMissing => {
          Err(remote_failure())
        },
        Behavior::Readable => Ok(Box::pin(std::io::Cursor::new(self.bytes.clone()))),
      }
    }

    async fn write_blob_if_absent(&self, _blob: &BlobDescriptor, _body: BlobReader) -> CacheResult<WriteOutcome> {
      match self.behavior {
        Behavior::Cancelled => Err(CacheError::Cancelled),
        Behavior::Failure | Behavior::InvalidMissing => Err(remote_failure()),
        Behavior::Conflict | Behavior::BlobConflict => Ok(WriteOutcome::Conflict),
        Behavior::Readable => Ok(WriteOutcome::AlreadyPresent),
      }
    }

    async fn write_action_if_absent(&self, _namespace: &str, _result: &ActionResultV1) -> CacheResult<WriteOutcome> {
      match self.behavior {
        Behavior::Cancelled => Err(CacheError::Cancelled),
        Behavior::Failure => Err(remote_failure()),
        Behavior::Conflict | Behavior::BlobConflict => Ok(WriteOutcome::Conflict),
        Behavior::Readable => Ok(WriteOutcome::AlreadyPresent),
        Behavior::InvalidMissing => panic!("action must not be published after an invalid missing-blob response"),
      }
    }
  }

  fn remote_failure() -> CacheError {
    CacheError::Remote {
      operation: "fixture",
      message: "unavailable".to_owned(),
    }
  }

  fn fixture() -> (BlobDescriptor, Vec<u8>, ActionResultV1) {
    let bytes = b"layered-cache".to_vec();
    let blob = BlobDescriptor {
      digest: Digest::blake3(&bytes),
      encoding: BlobEncoding::Identity,
      encoded_size_bytes: bytes.len() as u64,
      expanded_size_bytes: bytes.len() as u64,
      entry_count: 1,
    };
    let result = ActionResultV1 {
      result_version: ACTION_RESULT_VERSION_V1,
      action: Digest::blake3(b"layered-action"),
      output_bundle: Some(blob.clone()),
      stdout: None,
      task_outputs: Default::default(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    };
    (blob, bytes, result)
  }

  fn layered(root: &TempDir, behavior: Behavior, bytes: Vec<u8>) -> LayeredCacheStore {
    let local = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap());
    LayeredCacheStore::new(local, Arc::new(StubStore { behavior, bytes }))
  }

  #[tokio::test]
  async fn remote_lookup_failures_remain_visible_while_publication_degrades_to_local() {
    let root = TempDir::new().unwrap();
    let (blob, bytes, result) = fixture();
    let cache = layered(&root, Behavior::Failure, bytes.clone());
    assert!(format!("{cache:?}").contains("LayeredCacheStore"));
    assert!(matches!(
      cache.get_action("test", &result.action).await,
      Err(CacheError::Remote { .. })
    ));
    assert!(matches!(
      cache.find_missing_blobs(std::slice::from_ref(&blob)).await,
      Err(CacheError::Remote { .. })
    ));
    assert_eq!(
      cache
        .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
        .await
        .unwrap(),
      WriteOutcome::Written
    );
    assert_eq!(
      cache.write_action_if_absent("test", &result).await.unwrap(),
      WriteOutcome::Written
    );
    assert_eq!(
      cache.write_action_if_absent("test", &result).await.unwrap(),
      WriteOutcome::AlreadyPresent
    );
    let missing = BlobDescriptor {
      digest: Digest::blake3(b"missing"),
      encoded_size_bytes: 7,
      expanded_size_bytes: 7,
      ..blob
    };
    assert!(cache.read_blob(&missing).await.is_err());
  }

  #[tokio::test]
  async fn remote_cancellation_and_conflicts_preserve_their_meaning() {
    let cancelled_root = TempDir::new().unwrap();
    let (blob, bytes, result) = fixture();
    let cancelled = layered(&cancelled_root, Behavior::Cancelled, bytes.clone());
    assert!(matches!(
      cancelled.get_action("test", &result.action).await,
      Err(CacheError::Cancelled)
    ));
    assert!(matches!(
      cancelled.find_missing_blobs(std::slice::from_ref(&blob)).await,
      Err(CacheError::Cancelled)
    ));
    assert!(matches!(cancelled.read_blob(&blob).await, Err(CacheError::Cancelled)));

    let conflict_root = TempDir::new().unwrap();
    let conflict = layered(&conflict_root, Behavior::BlobConflict, bytes.clone());
    assert_eq!(
      conflict
        .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes.clone())))
        .await
        .unwrap(),
      WriteOutcome::Conflict
    );
    // Publish locally first so the action satisfies the local blob invariant.
    let local = LocalCacheStore::open(LocalCacheConfig::new(conflict_root.path())).unwrap();
    local
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
      .await
      .unwrap();
    assert_eq!(
      conflict.write_action_if_absent("test", &result).await.unwrap(),
      WriteOutcome::Conflict
    );
    assert!(local.get_action("test", &result.action).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn remote_hits_are_lazy_and_populate_the_blob_only_when_read() {
    let root = TempDir::new().unwrap();
    let (blob, bytes, result) = fixture();
    let local = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap());
    let remote = Arc::new(HitStore {
      result: result.clone(),
      bytes: bytes.clone(),
      reads: AtomicUsize::new(0),
      missing_queries: AtomicUsize::new(0),
      blob_writes: AtomicUsize::new(0),
      action_writes: AtomicUsize::new(0),
    });
    let cache = LayeredCacheStore::new(local.clone(), remote.clone());

    let lookup = cache.get_action("test", &result.action).await.unwrap().unwrap();
    assert_eq!(lookup.layer, CacheLayer::Remote);
    assert_eq!(remote.reads.load(Ordering::Relaxed), 0);

    let mut first = Vec::new();
    cache
      .read_blob(&blob)
      .await
      .unwrap()
      .read_to_end(&mut first)
      .await
      .unwrap();
    let mut second = Vec::new();
    cache
      .read_blob(&blob)
      .await
      .unwrap()
      .read_to_end(&mut second)
      .await
      .unwrap();
    assert_eq!(first, bytes);
    assert_eq!(second, bytes);
    assert_eq!(remote.reads.load(Ordering::Relaxed), 1);
    assert!(local
      .find_missing_blobs(std::slice::from_ref(&blob))
      .await
      .unwrap()
      .is_empty());

    cache.commit_verified_hit("test", &result).await.unwrap();
    assert!(local.get_action("test", &result.action).await.unwrap().is_some());
    assert_eq!(remote.action_writes.load(Ordering::Relaxed), 0);

    // Exercise the rest of the tier boundary after the L1 population: the
    // shared store still reports global readability and idempotent writes.
    assert!(cache
      .find_missing_blobs(std::slice::from_ref(&blob))
      .await
      .unwrap()
      .is_empty());
    assert_eq!(
      cache
        .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
        .await
        .unwrap(),
      WriteOutcome::AlreadyPresent
    );
    assert_eq!(
      cache.write_action_if_absent("test", &result).await.unwrap(),
      WriteOutcome::AlreadyPresent
    );
    assert_eq!(
      cache.write_action_if_absent("test", &result).await.unwrap(),
      WriteOutcome::AlreadyPresent
    );
  }

  #[tokio::test]
  async fn a_blob_available_in_either_tier_is_not_a_layered_miss() {
    let root = TempDir::new().unwrap();
    let (blob, bytes, _) = fixture();
    let cache = layered(&root, Behavior::Readable, bytes.clone());
    cache
      .local
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
      .await
      .unwrap();
    assert!(cache.find_missing_blobs(&[blob]).await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn missing_local_blob_can_stream_from_remote() {
    let root = TempDir::new().unwrap();
    let (blob, bytes, result) = fixture();
    let cache = layered(&root, Behavior::Readable, bytes.clone());
    let mut restored = Vec::new();
    cache
      .read_blob(&blob)
      .await
      .unwrap()
      .read_to_end(&mut restored)
      .await
      .unwrap();
    assert_eq!(restored, bytes);
    assert_eq!(
      cache
        .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
        .await
        .unwrap(),
      WriteOutcome::AlreadyPresent
    );
    assert_eq!(
      cache.write_action_if_absent("test", &result).await.unwrap(),
      WriteOutcome::Written
    );
  }

  #[tokio::test]
  async fn verified_remote_hit_replaces_a_conflicting_local_action_without_touching_l2() {
    let root = TempDir::new().unwrap();
    let (blob, bytes, result) = fixture();
    let local = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap());
    local
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes.clone())))
      .await
      .unwrap();
    let mut conflicting = result.clone();
    conflicting.stdout = Some("stale".to_owned());
    local.write_action_if_absent("test", &conflicting).await.unwrap();
    let remote = Arc::new(HitStore {
      result: result.clone(),
      bytes,
      reads: AtomicUsize::new(0),
      missing_queries: AtomicUsize::new(0),
      blob_writes: AtomicUsize::new(0),
      action_writes: AtomicUsize::new(0),
    });
    let cache = LayeredCacheStore::new(local.clone(), remote.clone());

    cache.commit_verified_hit("test", &result).await.unwrap();

    assert_eq!(
      local.get_action("test", &result.action).await.unwrap().unwrap().result,
      result
    );
    assert_eq!(remote.missing_queries.load(Ordering::Relaxed), 0);
    assert_eq!(remote.blob_writes.load(Ordering::Relaxed), 0);
    assert_eq!(remote.action_writes.load(Ordering::Relaxed), 0);
  }

  #[tokio::test]
  async fn remote_publication_preserves_conflicts_and_softens_invalid_l2_blob_queries() {
    let no_bundle_root = TempDir::new().unwrap();
    let (_, bytes, mut no_bundle) = fixture();
    no_bundle.output_bundle = None;
    let conflict = layered(&no_bundle_root, Behavior::Conflict, bytes.clone());
    assert_eq!(
      conflict.write_action_if_absent("test", &no_bundle).await.unwrap(),
      WriteOutcome::Conflict
    );

    let conflict_root = TempDir::new().unwrap();
    let (blob, bytes, _) = fixture();
    let blob_conflict = layered(&conflict_root, Behavior::BlobConflict, bytes.clone());
    assert_eq!(
      blob_conflict
        .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes.clone())))
        .await
        .unwrap(),
      WriteOutcome::Conflict
    );

    let malformed_root = TempDir::new().unwrap();
    let malformed = layered(&malformed_root, Behavior::InvalidMissing, bytes.clone());
    assert_eq!(
      malformed
        .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
        .await
        .unwrap(),
      WriteOutcome::Written
    );
    assert!(malformed
      .local
      .find_missing_blobs(std::slice::from_ref(&blob))
      .await
      .unwrap()
      .is_empty());
  }
}
