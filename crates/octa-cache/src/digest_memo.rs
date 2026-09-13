//! Local CAS-backed accelerator for unchanged input snapshots.
//!
//! The portable cache key always contains BLAKE3 content digests. This module
//! stores the resulting canonical input entries under a separate key derived
//! from local file identities and timestamps. A metadata mismatch is a memo
//! miss and falls back to full content hashing. Records use the existing local
//! action/blob store, so its atomic publication, corruption checks, capacity
//! accounting, locking, and garbage collection remain the only persistence
//! implementation in `octa-cache`.

use std::{
  collections::BTreeMap,
  io::Cursor,
  sync::{Arc, Weak},
};

use octa_cache_protocol::{
  ActionResultV1, BlobDescriptor, BlobEncoding, Digest, DigestAlgorithm, ACTION_RESULT_VERSION_V1,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::{input::InputEntry, CacheError, CacheResult, CacheStore, LocalCacheStore, WriteOutcome};

const MEMO_NAMESPACE: &str = "octa.input-snapshot-memo.v1";
const MEMO_VERSION: u16 = 1;
// Avoid reserving an attacker-controlled declared size up front. Larger valid
// documents grow normally and remain bounded by `SnapshotOptions`.
const INITIAL_MEMO_CAPACITY_BYTES: u64 = 16 * 1024 * 1024;

/// Shared local store and process-level coalescing for equivalent snapshots.
#[derive(Clone)]
pub(crate) struct DigestMemo {
  store: Arc<LocalCacheStore>,
  // Weak entries prevent an unbounded process-lifetime map when a long-lived
  // runtime observes many distinct source revisions.
  in_flight: Arc<Mutex<BTreeMap<Digest, Weak<Mutex<()>>>>>,
}

/// Validated canonical entries recovered from a local memo blob.
pub(crate) struct MemoSnapshot {
  pub(crate) root: Digest,
  pub(crate) entries: Vec<InputEntry>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MemoDocument {
  version: u16,
  root: Digest,
  entries: Vec<InputEntry>,
}

#[derive(Serialize)]
struct MemoDocumentRef<'a> {
  version: u16,
  root: Digest,
  entries: &'a [InputEntry],
}

/// Serialization sink that stops before an oversized memo can inflate memory.
struct BoundedMemoBytes {
  bytes: Vec<u8>,
  max: usize,
  exceeded: bool,
}

impl BoundedMemoBytes {
  fn new(max_bytes: u64) -> Self {
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    Self {
      bytes: Vec::with_capacity(max.min(INITIAL_MEMO_CAPACITY_BYTES as usize)),
      max,
      exceeded: false,
    }
  }
}

impl std::io::Write for BoundedMemoBytes {
  fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
    if self
      .bytes
      .len()
      .checked_add(buffer.len())
      .is_none_or(|length| length > self.max)
    {
      self.exceeded = true;
      return Err(std::io::Error::other(
        "input snapshot memo exceeds its configured limit",
      ));
    }
    self.bytes.extend_from_slice(buffer);
    Ok(buffer.len())
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

impl DigestMemo {
  pub(crate) fn new(store: Arc<LocalCacheStore>) -> Self {
    Self {
      store,
      in_flight: Arc::new(Mutex::new(BTreeMap::new())),
    }
  }

  /// Serializes equivalent snapshots until the first caller publishes a memo.
  pub(crate) async fn lock(&self, metadata: Digest) -> OwnedMutexGuard<()> {
    let lock = {
      let mut in_flight = self.in_flight.lock().await;
      // Completed snapshots leave only a weak reference. Removing those keys
      // on the next registration keeps the table proportional to snapshots
      // that are actually active, without a second cleanup task.
      in_flight.retain(|_, lock| lock.strong_count() != 0);
      match in_flight.get(&metadata).and_then(Weak::upgrade) {
        Some(lock) => lock,
        None => {
          let lock = Arc::new(Mutex::new(()));
          in_flight.insert(metadata, Arc::downgrade(&lock));
          lock
        },
      }
    };
    lock.lock_owned().await
  }

  /// Loads and verifies an immutable memo record.
  pub(crate) async fn get(
    &self,
    metadata: Digest,
    max_entries: usize,
    max_bytes: u64,
  ) -> CacheResult<Option<MemoSnapshot>> {
    let Some(lookup) = self.store.get_action(MEMO_NAMESPACE, &metadata).await? else {
      return Ok(None);
    };
    let result = lookup.result;
    if result.action != metadata
      || result.stdout.is_some()
      || !result.task_outputs.is_empty()
      || !result.artifacts.is_empty()
      || !result.reports.is_empty()
    {
      return Ok(None);
    }
    let Some(blob) = result.output_bundle else {
      return Ok(None);
    };
    if blob.encoding != BlobEncoding::Identity
      || blob.encoded_size_bytes > max_bytes
      || blob.entry_count > max_entries as u64
    {
      return Ok(None);
    }
    let reader = self.store.read_blob(&blob).await?;
    let mut bytes = Vec::with_capacity(blob.encoded_size_bytes.min(INITIAL_MEMO_CAPACITY_BYTES) as usize);
    reader
      .take(blob.encoded_size_bytes.saturating_add(1))
      .read_to_end(&mut bytes)
      .await
      .map_err(|error| CacheError::InvalidBundle(format!("cannot read input snapshot memo: {error}")))?;
    let actual = Digest::blake3(&bytes);
    if bytes.len() as u64 != blob.encoded_size_bytes || actual != blob.digest {
      return Ok(None);
    }
    let document: MemoDocument = match serde_json::from_slice(&bytes) {
      Ok(document) => document,
      Err(_) => return Ok(None),
    };
    if document.version != MEMO_VERSION
      || document.entries.is_empty()
      || document.entries.len() > max_entries
      || document.root.algorithm() != DigestAlgorithm::Blake3
    {
      return Ok(None);
    }
    Ok(Some(MemoSnapshot {
      root: document.root,
      entries: document.entries,
    }))
  }

  /// Publishes one fully hashed snapshot through the existing local CAS.
  pub(crate) async fn put(
    &self,
    metadata: Digest,
    root: Digest,
    entries: &[InputEntry],
    max_bytes: u64,
  ) -> CacheResult<()> {
    if entries.is_empty() {
      return Ok(());
    }
    // Serialize through a borrowed wire view. Large source trees can contain
    // hundreds of thousands of entries, so cloning them merely to write the
    // memo would noticeably inflate the cold path's memory and CPU cost.
    let mut encoded = BoundedMemoBytes::new(max_bytes);
    let serialization = serde_json::to_writer(
      &mut encoded,
      &MemoDocumentRef {
        version: MEMO_VERSION,
        root,
        entries,
      },
    );
    if encoded.exceeded {
      return Ok(());
    }
    serialization.map_err(|error| CacheError::Configuration(format!("cannot encode input snapshot memo: {error}")))?;
    let bytes = encoded.bytes;
    let digest = Digest::blake3(&bytes);
    let blob = BlobDescriptor {
      digest,
      encoding: BlobEncoding::Identity,
      encoded_size_bytes: bytes.len() as u64,
      expanded_size_bytes: bytes.len() as u64,
      entry_count: entries.len() as u64,
    };
    match self
      .store
      .write_blob_if_absent(&blob, Box::pin(Cursor::new(bytes)))
      .await?
    {
      WriteOutcome::Written | WriteOutcome::AlreadyPresent => {},
      WriteOutcome::Conflict => return Ok(()),
    }
    let result = ActionResultV1 {
      result_version: ACTION_RESULT_VERSION_V1,
      action: metadata,
      output_bundle: Some(blob),
      stdout: None,
      task_outputs: BTreeMap::new(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    };
    result.validate()?;
    let _ = self.store.write_action_if_absent(MEMO_NAMESPACE, &result).await?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use octa_cache_protocol::RelativePath;
  use tempfile::TempDir;

  use super::*;
  use crate::{LocalCacheConfig, LocalCacheStore};

  async fn publish_memo_bytes(store: &Arc<LocalCacheStore>, metadata: Digest, bytes: Vec<u8>, entry_count: u64) {
    let blob = BlobDescriptor {
      digest: Digest::blake3(&bytes),
      encoding: BlobEncoding::Identity,
      encoded_size_bytes: bytes.len() as u64,
      expanded_size_bytes: bytes.len() as u64,
      entry_count,
    };
    assert_eq!(
      store
        .write_blob_if_absent(&blob, Box::pin(Cursor::new(bytes)))
        .await
        .unwrap(),
      WriteOutcome::Written
    );
    let result = ActionResultV1 {
      result_version: ACTION_RESULT_VERSION_V1,
      action: metadata,
      output_bundle: Some(blob),
      stdout: None,
      task_outputs: BTreeMap::new(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    };
    assert_eq!(
      store.write_action_if_absent(MEMO_NAMESPACE, &result).await.unwrap(),
      WriteOutcome::Written
    );
  }

  fn snapshot(contents: &[u8]) -> MemoSnapshot {
    let entries = vec![InputEntry::File {
      path: RelativePath::new("src/input").unwrap(),
      content: Digest::blake3(contents),
      executable: false,
    }];
    MemoSnapshot {
      root: Digest::blake3(b"root"),
      entries,
    }
  }

  #[tokio::test]
  async fn local_cas_round_trips_an_input_snapshot_memo() {
    let root = TempDir::new().unwrap();
    let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap());
    let memo = DigestMemo::new(store);
    let metadata = Digest::blake3(b"metadata");
    let expected = snapshot(b"contents");

    memo
      .put(metadata, expected.root, &expected.entries, 1024 * 1024)
      .await
      .unwrap();
    let actual = memo.get(metadata, 10, 1024 * 1024).await.unwrap().unwrap();

    assert_eq!(actual.root, expected.root);
    assert_eq!(actual.entries, expected.entries);
    assert!(memo
      .get(Digest::blake3(b"different metadata"), 10, 1024 * 1024)
      .await
      .unwrap()
      .is_none());
  }

  #[tokio::test]
  async fn malformed_or_out_of_bounds_memos_are_optimization_misses() {
    let root = TempDir::new().unwrap();
    let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap());
    let memo = DigestMemo::new(store);
    let metadata = Digest::blake3(b"metadata");

    let expected = snapshot(b"contents");
    memo
      .put(metadata, expected.root, &expected.entries, 1024 * 1024)
      .await
      .unwrap();

    assert!(memo.get(metadata, 0, 1024 * 1024).await.unwrap().is_none());
    assert!(memo.get(metadata, 10, 1).await.unwrap().is_none());

    let malformed = Digest::blake3(b"malformed-metadata");
    publish_memo_bytes(&memo.store, malformed, b"not JSON".to_vec(), 1).await;
    assert!(memo.get(malformed, 1, 1024 * 1024).await.unwrap().is_none());

    let wrong_version = Digest::blake3(b"wrong-version-metadata");
    let bytes = serde_json::to_vec(&MemoDocumentRef {
      version: MEMO_VERSION + 1,
      root: Digest::blake3(b"root"),
      entries: &snapshot(b"contents").entries,
    })
    .unwrap();
    publish_memo_bytes(&memo.store, wrong_version, bytes, 1).await;
    assert!(memo.get(wrong_version, 1, 1024 * 1024).await.unwrap().is_none());

    let missing_blob = Digest::blake3(b"missing-blob-metadata");
    let empty_result = ActionResultV1 {
      result_version: ACTION_RESULT_VERSION_V1,
      action: missing_blob,
      output_bundle: None,
      stdout: None,
      task_outputs: BTreeMap::new(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    };
    assert_eq!(
      memo
        .store
        .write_action_if_absent(MEMO_NAMESPACE, &empty_result)
        .await
        .unwrap(),
      WriteOutcome::Written
    );
    assert!(memo.get(missing_blob, 1, 1024 * 1024).await.unwrap().is_none());

    let unexpected_metadata = Digest::blake3(b"unexpected-result-metadata");
    let result_with_stdout = ActionResultV1 {
      result_version: ACTION_RESULT_VERSION_V1,
      action: unexpected_metadata,
      output_bundle: None,
      stdout: Some("not a memo".to_owned()),
      task_outputs: BTreeMap::new(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    };
    memo
      .store
      .write_action_if_absent(MEMO_NAMESPACE, &result_with_stdout)
      .await
      .unwrap();
    assert!(memo.get(unexpected_metadata, 1, 1024 * 1024).await.unwrap().is_none());
  }

  #[tokio::test]
  async fn equivalent_snapshots_share_one_publication_lock_and_empty_memos_are_skipped() {
    let root = TempDir::new().unwrap();
    let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap());
    let memo = DigestMemo::new(store);
    let metadata = Digest::blake3(b"metadata");
    let first = memo.lock(metadata).await;

    let contender = {
      let memo = memo.clone();
      tokio::spawn(async move { memo.lock(metadata).await })
    };
    tokio::task::yield_now().await;
    assert!(!contender.is_finished());
    drop(first);
    drop(contender.await.unwrap());

    memo
      .put(metadata, Digest::blake3(b"empty"), &[], 1024 * 1024)
      .await
      .unwrap();
    assert!(memo.get(metadata, 1, 1024 * 1024).await.unwrap().is_none());

    let oversized = Digest::blake3(b"oversized");
    let snapshot = snapshot(b"contents");
    memo.put(oversized, snapshot.root, &snapshot.entries, 1).await.unwrap();
    assert!(memo.get(oversized, 1, 1024 * 1024).await.unwrap().is_none());
  }
}
