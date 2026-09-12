//! Local-store concurrency, integrity, and retention tests.

use std::{
  collections::BTreeMap,
  io::{self, Cursor},
  pin::Pin,
  process::Command,
  sync::Arc,
  task::{Context, Poll},
  time::Duration,
};

use octa_cache_protocol::{ActionResultV1, BlobDescriptor, BlobEncoding, Digest, DigestAlgorithm};
use serde_json::json;
use tokio::io::{AsyncRead, ReadBuf};

use super::*;
use crate::{BundleLimits, RestoreManager};

const CHILD_ROOT: &str = "OCTA_CACHE_PROCESS_WRITER_ROOT";

fn process_blob_bytes() -> Vec<u8> {
  (0..1024 * 1024).map(|index| (index % 251) as u8).collect()
}

fn blob(bytes: &[u8]) -> BlobDescriptor {
  BlobDescriptor {
    digest: Digest::new(
      DigestAlgorithm::Blake3,
      *blake3::hash(bytes).as_bytes(),
      bytes.len() as u64,
    ),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: bytes.len() as u64,
    expanded_size_bytes: bytes.len() as u64,
    entry_count: 1,
  }
}

fn result(action: Digest, bundle: Option<BlobDescriptor>, stdout: &str) -> ActionResultV1 {
  ActionResultV1 {
    result_version: 1,
    action,
    output_bundle: bundle,
    stdout: Some(stdout.to_owned()),
    task_outputs: BTreeMap::from([("answer".to_owned(), json!(42))]),
    artifacts: Vec::new(),
    reports: Vec::new(),
  }
}

fn action(byte: u8) -> Digest {
  Digest::new(DigestAlgorithm::Blake3, [byte; 32], 100)
}

fn reader(bytes: &[u8]) -> BlobReader {
  Box::pin(Cursor::new(bytes.to_vec()))
}

struct FailingReader;

impl AsyncRead for FailingReader {
  fn poll_read(self: Pin<&mut Self>, _context: &mut Context<'_>, _buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    Poll::Ready(Err(io::Error::other("injected source failure")))
  }
}

#[tokio::test]
async fn publishes_immutable_blobs_and_reports_action_conflicts() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let bytes = b"canonical bundle";
  let descriptor = blob(bytes);

  assert_eq!(
    store.write_blob_if_absent(&descriptor, reader(bytes)).await.unwrap(),
    WriteOutcome::Written
  );
  assert!(store
    .find_missing_blobs(std::slice::from_ref(&descriptor))
    .await
    .unwrap()
    .is_empty());
  assert_eq!(
    store.write_blob_if_absent(&descriptor, reader(bytes)).await.unwrap(),
    WriteOutcome::AlreadyPresent
  );

  let original = result(action(1), Some(descriptor.clone()), "first");
  assert_eq!(
    store.write_action_if_absent("project/main", &original).await.unwrap(),
    WriteOutcome::Written
  );
  assert_eq!(
    store
      .get_action("project/main", &action(1))
      .await
      .unwrap()
      .map(|lookup| lookup.result),
    Some(original.clone())
  );
  assert_eq!(
    store
      .get_action("project/main", &action(1))
      .await
      .unwrap()
      .unwrap()
      .layer,
    octa_cache_protocol::CacheLayer::Local
  );
  assert_eq!(
    store.write_action_if_absent("project/main", &original).await.unwrap(),
    WriteOutcome::AlreadyPresent
  );

  let conflicting = result(action(1), Some(descriptor), "different");
  assert_eq!(
    store
      .write_action_if_absent("project/main", &conflicting)
      .await
      .unwrap(),
    WriteOutcome::Conflict
  );
}

#[test]
fn validates_local_configuration_and_exposes_composed_paths() {
  let root = tempfile::tempdir().unwrap();
  let mut config = LocalCacheConfig::new(root.path());
  config.low_watermark_bytes = config.high_watermark_bytes + 1;
  assert!(matches!(
    LocalCacheStore::open(config),
    Err(CacheError::Configuration(_))
  ));

  let mut config = LocalCacheConfig::new(root.path());
  config.temporary_grace = Duration::ZERO;
  assert!(matches!(
    LocalCacheStore::open(config),
    Err(CacheError::Configuration(_))
  ));

  let mut config = LocalCacheConfig::new(root.path());
  config.max_blob_compression_ratio = 0;
  assert!(matches!(
    LocalCacheStore::open(config),
    Err(CacheError::Configuration(_))
  ));

  let mut config = LocalCacheConfig::new(root.path());
  config.max_entries = 0;
  assert!(matches!(
    LocalCacheStore::open(config),
    Err(CacheError::Configuration(_))
  ));

  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  assert_eq!(store.layout_root(), root.path().join("v1"));
  RestoreManager::open(store.layout_root(), BundleLimits::default()).unwrap();
  let action_path = store.action_path("ns", &action(1)).unwrap();
  assert_eq!(
    action_path.parent().unwrap().file_name().unwrap(),
    &action(1).hex()[..2]
  );
  assert!(matches!(
    store.action_path("", &action(1)),
    Err(CacheError::Protocol(_))
  ));
  assert!(matches!(
    store.action_path("bad\nnamespace", &action(1)),
    Err(CacheError::Protocol(_))
  ));
  let sha = Digest::new(DigestAlgorithm::Sha256, [1; 32], 1);
  assert!(matches!(
    store.action_path("ns", &sha),
    Err(CacheError::Configuration(_))
  ));
  assert!(matches!(
    LocalCacheStore::open(LocalCacheConfig::new("relative-cache")),
    Err(CacheError::Configuration(_))
  ));
}

#[test]
fn rejects_files_at_cache_directory_boundaries() {
  let parent = tempfile::tempdir().unwrap();
  let root_file = parent.path().join("cache-file");
  fs::write(&root_file, b"not a directory").unwrap();
  assert!(matches!(
    LocalCacheStore::open(LocalCacheConfig::new(&root_file)),
    Err(CacheError::Configuration(_))
  ));

  let root = tempfile::tempdir().unwrap();
  fs::create_dir(root.path().join("v1")).unwrap();
  fs::write(root.path().join("v1/actions"), b"not a directory").unwrap();
  assert!(matches!(
    LocalCacheStore::open(LocalCacheConfig::new(root.path())),
    Err(CacheError::Configuration(_))
  ));
}

#[tokio::test]
async fn recovers_capacity_state_and_rejects_unsafe_replacements() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let state = store.layout.join("capacity-v1");

  // A torn fixed-width state is rebuilt from the bounded filesystem scan.
  fs::write(&state, [1_u8]).unwrap();
  let recovered = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  assert_eq!(
    recovered.status().await.unwrap().used_bytes,
    gc::directory_bytes(&recovered.layout, recovered.config.max_entries).unwrap()
  );

  // Arithmetic overflow must fail closed rather than wrap the shared quota.
  recovered.capacity.reconcile(u64::MAX).unwrap();
  assert!(matches!(
    recovered.capacity.try_reserve(1, u64::MAX).await,
    Err(CacheError::Limit(_))
  ));

  // Replacing the state leaf after opening cannot redirect a later update.
  fs::remove_file(&state).unwrap();
  fs::create_dir(&state).unwrap();
  assert!(matches!(
    recovered.capacity.reconcile(0),
    Err(CacheError::Configuration(_))
  ));
  assert!(matches!(
    LocalCacheStore::open(LocalCacheConfig::new(root.path())),
    Err(CacheError::Configuration(_))
  ));
}

#[cfg(unix)]
#[test]
fn replaces_a_linked_capacity_state_without_touching_its_target() {
  use std::os::unix::fs::symlink;

  let root = tempfile::tempdir().unwrap();
  let outside = tempfile::NamedTempFile::new().unwrap();
  fs::write(outside.path(), b"operator data").unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let state = store.layout.join("capacity-v1");
  fs::remove_file(&state).unwrap();
  symlink(outside.path(), &state).unwrap();

  LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  assert_eq!(fs::read(outside.path()).unwrap(), b"operator data");
  assert!(!fs::symlink_metadata(state).unwrap().file_type().is_symlink());
}

#[tokio::test]
async fn collection_can_make_room_between_capacity_checks() {
  let root = tempfile::tempdir().unwrap();
  let mut config = LocalCacheConfig::new(root.path());
  config.max_bytes = 1024 * 1024;
  config.high_watermark_bytes = 0;
  config.low_watermark_bytes = 0;
  let store = LocalCacheStore::open(config).unwrap();

  let bytes = b"accepted after collection";
  assert_eq!(
    store.write_blob_if_absent(&blob(bytes), reader(bytes)).await.unwrap(),
    WriteOutcome::Written
  );
}

#[tokio::test]
async fn capacity_is_shared_by_independent_store_instances_without_rescanning() {
  let root = tempfile::tempdir().unwrap();
  let initial = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let baseline = initial.status().await.unwrap().used_bytes;
  drop(initial);

  // A valid fixed-width ledger lets a later short-lived runner open even when
  // a full directory scan would exceed this deliberately tiny entry budget.
  fs::write(root.path().join("v1/tmp/unaccounted-fixture"), b"fixture").unwrap();
  let mut config = LocalCacheConfig::new(root.path());
  config.max_entries = 1;
  config.max_bytes = baseline + 6;
  config.high_watermark_bytes = config.max_bytes;
  config.low_watermark_bytes = config.max_bytes;
  let first = LocalCacheStore::open(config.clone()).unwrap();
  let second = LocalCacheStore::open(config).unwrap();

  let one = blob(b"one!");
  assert_eq!(
    first.write_blob_if_absent(&one, reader(b"one!")).await.unwrap(),
    WriteOutcome::Written
  );
  assert_eq!(
    first.status().await.unwrap().used_bytes,
    second.status().await.unwrap().used_bytes
  );

  let two = blob(b"two!");
  assert!(matches!(
    second.write_blob_if_absent(&two, reader(b"two!")).await,
    Err(CacheError::Limit(_))
  ));
}

#[cfg(unix)]
#[test]
fn rejects_linked_cache_roots_and_fixed_layout_directories() {
  use std::os::unix::fs::symlink;

  let parent = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  let linked_root = parent.path().join("linked-cache");
  symlink(outside.path(), &linked_root).unwrap();
  assert!(matches!(
    LocalCacheStore::open(LocalCacheConfig::new(&linked_root)),
    Err(CacheError::Configuration(_))
  ));

  let root = parent.path().join("cache");
  fs::create_dir_all(root.join("v1")).unwrap();
  symlink(outside.path(), root.join("v1/actions")).unwrap();
  assert!(matches!(
    LocalCacheStore::open(LocalCacheConfig::new(&root)),
    Err(CacheError::Configuration(_))
  ));
}

#[tokio::test]
async fn reports_absent_and_malformed_blob_streams_without_publication() {
  let root = tempfile::tempdir().unwrap();
  let mut config = LocalCacheConfig::new(root.path());
  config.max_bytes = 32;
  config.high_watermark_bytes = 32;
  config.low_watermark_bytes = 16;
  let store = LocalCacheStore::open(config).unwrap();
  let descriptor = blob(b"expected");

  assert_eq!(
    store
      .find_missing_blobs(std::slice::from_ref(&descriptor))
      .await
      .unwrap(),
    vec![descriptor.clone()]
  );
  assert!(matches!(store.read_blob(&descriptor).await, Err(CacheError::Io { .. })));
  assert!(matches!(
    store.write_blob_if_absent(&descriptor, reader(b"short")).await,
    Err(CacheError::InvalidBundle(_))
  ));
  assert!(matches!(
    store
      .write_blob_if_absent(&descriptor, reader(b"expected-and-extra"))
      .await,
    Err(CacheError::InvalidBundle(_))
  ));
  assert!(matches!(
    store.write_blob_if_absent(&descriptor, Box::pin(FailingReader)).await,
    Err(CacheError::Io { .. })
  ));
  assert_eq!(fs::read_dir(store.layout.join("tmp")).unwrap().count(), 0);

  let oversized = BlobDescriptor {
    digest: Digest::new(DigestAlgorithm::Blake3, [1; 32], 33),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: 33,
    expanded_size_bytes: 33,
    entry_count: 1,
  };
  assert!(matches!(
    store.write_blob_if_absent(&oversized, reader(&[0; 33])).await,
    Err(CacheError::Limit(_))
  ));
}

#[tokio::test]
async fn rejects_a_write_when_recent_state_keeps_the_cache_over_capacity() {
  let root = tempfile::tempdir().unwrap();
  let initial = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  fs::write(initial.layout.join("tmp/recent-state.tmp"), [0_u8; 128]).unwrap();
  let usage = gc::directory_bytes(&initial.layout, initial.config.max_entries).unwrap();

  let mut config = initial.config.clone();
  config.max_bytes = usage + 4;
  config.high_watermark_bytes = 0;
  config.low_watermark_bytes = 0;
  let constrained = LocalCacheStore::open(config).unwrap();
  let bytes = b"capacity";
  assert!(matches!(
    constrained.write_blob_if_absent(&blob(bytes), reader(bytes)).await,
    Err(CacheError::Limit(message)) if message.contains("remains full")
  ));
  assert!(constrained.layout.join("tmp/recent-state.tmp").exists());
}

#[tokio::test]
async fn missing_lookup_and_repeated_quarantine_are_idempotent() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  assert!(store.get_action("ns", &action(7)).await.unwrap().is_none());
  store.quarantine_blob(&blob(b"absent"), "already absent").await.unwrap();

  // Exercise the second absence window: the derived parent exists, but the
  // object disappears before quarantine inspects the final leaf.
  let absent = store.layout.join("blobs/blake3/missing");
  store
    .quarantine(absent, "removed concurrently".to_owned())
    .await
    .unwrap();
}

#[tokio::test]
async fn capacity_release_failures_retain_the_primary_publication_error() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let state = store.layout.join("capacity-v1");
  fs::remove_file(&state).unwrap();
  fs::create_dir(&state).unwrap();

  assert!(matches!(
    store.finish_reservation(1, Ok(WriteOutcome::AlreadyPresent)).await,
    Err(CacheError::Metadata(_))
  ));
  assert!(matches!(
    store
      .finish_reservation(1, Err(CacheError::Limit("primary".to_owned())))
      .await,
    Err(CacheError::CapacityRelease { primary, release })
      if matches!(*primary, CacheError::Limit(_)) && matches!(*release, CacheError::Metadata(_))
  ));
}

#[tokio::test]
async fn action_publication_distinguishes_a_missing_blob_leaf() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let descriptor = blob(b"missing leaf");
  fs::create_dir_all(store.blob_path(&descriptor).parent().unwrap()).unwrap();
  let logical = result(action(15), Some(descriptor), "missing");

  assert!(matches!(
    store.write_action_if_absent("ns", &logical).await,
    Err(CacheError::Corrupt { reason, .. }) if reason.contains("missing blob")
  ));
}

#[cfg(unix)]
#[tokio::test]
async fn filesystem_publication_failures_leave_no_false_hits() {
  use std::os::unix::fs::PermissionsExt as _;

  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let temporary_root = store.layout.join("tmp");
  fs::set_permissions(&temporary_root, fs::Permissions::from_mode(0o500)).unwrap();
  let descriptor = blob(b"blocked");
  assert!(matches!(
    store.write_blob_if_absent(&descriptor, reader(b"blocked")).await,
    Err(CacheError::Io { .. })
  ));
  let logical = result(action(10), None, "blocked");
  assert!(matches!(
    store.write_action_if_absent("ns", &logical).await,
    Err(CacheError::Io { .. })
  ));
  fs::set_permissions(&temporary_root, fs::Permissions::from_mode(0o700)).unwrap();
  assert!(!store.blob_path(&descriptor).exists());
  assert!(!store.action_path("ns", &logical.action).unwrap().exists());

  let corrupt_path = store.action_path("ns", &action(11)).unwrap();
  fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
  fs::write(&corrupt_path, b"invalid").unwrap();
  let quarantine = store.layout.join("quarantine");
  fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o500)).unwrap();
  assert!(matches!(
    store.get_action("ns", &action(11)).await,
    Err(CacheError::Io { .. })
  ));
  fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700)).unwrap();
  assert!(corrupt_path.exists());
}

#[tokio::test]
async fn rejects_a_non_file_action_destination_and_cleans_its_temporary_file() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let logical = result(action(14), None, "directory collision");
  let destination = store.action_path("ns", &logical.action).unwrap();
  fs::create_dir_all(&destination).unwrap();

  assert!(matches!(
    store.write_action_if_absent("ns", &logical).await,
    Err(CacheError::Corrupt { .. })
  ));
  assert_eq!(fs::read_dir(store.layout.join("tmp")).unwrap().count(), 0);
}

#[test]
fn dynamic_parent_validation_rejects_paths_outside_the_layout() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();

  assert!(matches!(
    validate_cache_parent(&store.layout, root.path(), false),
    Err(CacheError::Configuration(_))
  ));
  assert!(matches!(
    validate_cache_parent(&store.layout, &store.layout.join("actions/../escape/object"), false),
    Err(CacheError::Configuration(_))
  ));
}

#[tokio::test]
async fn keeps_physical_encodings_with_distinct_sizes_separate() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let canonical = (0..32_768).map(|index| (index % 251) as u8).collect::<Vec<_>>();
  let compressed = zstd::stream::encode_all(&canonical[..], 1).unwrap();
  let semantic = Digest::new(
    DigestAlgorithm::Blake3,
    *blake3::hash(&canonical).as_bytes(),
    canonical.len() as u64,
  );
  let first = BlobDescriptor {
    digest: semantic,
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: canonical.len() as u64,
    expanded_size_bytes: canonical.len() as u64,
    entry_count: 1,
  };
  let second = BlobDescriptor {
    encoding: BlobEncoding::ZstdV1,
    encoded_size_bytes: compressed.len() as u64,
    ..first.clone()
  };
  store.write_blob_if_absent(&first, reader(&canonical)).await.unwrap();
  store.write_blob_if_absent(&second, reader(&compressed)).await.unwrap();
  assert!(store.find_missing_blobs(&[first, second]).await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_malformed_and_over_expanding_zstd_representations() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let malformed = b"not a zstd stream";
  let malformed_descriptor = BlobDescriptor {
    digest: Digest::new(DigestAlgorithm::Blake3, [7; 32], 4),
    encoding: BlobEncoding::ZstdV1,
    encoded_size_bytes: malformed.len() as u64,
    expanded_size_bytes: 4,
    entry_count: 1,
  };
  assert!(matches!(
    store
      .write_blob_if_absent(&malformed_descriptor, reader(malformed))
      .await,
    Err(CacheError::InvalidBundle(_))
  ));

  let expanded = b"larger than declared";
  let compressed = zstd::stream::encode_all(&expanded[..], 1).unwrap();
  let over_expanding = BlobDescriptor {
    digest: Digest::new(DigestAlgorithm::Blake3, *blake3::hash(b"small").as_bytes(), 5),
    encoding: BlobEncoding::ZstdV1,
    encoded_size_bytes: compressed.len() as u64,
    expanded_size_bytes: 5,
    entry_count: 1,
  };
  assert!(matches!(
    store.write_blob_if_absent(&over_expanding, reader(&compressed)).await,
    Err(CacheError::InvalidBundle(_))
  ));

  let mut limited = LocalCacheConfig::new(root.path().join("limited"));
  limited.max_expanded_blob_bytes = 8;
  let limited = LocalCacheStore::open(limited).unwrap();
  let too_large = BlobDescriptor {
    digest: Digest::new(DigestAlgorithm::Blake3, [1; 32], 9),
    encoding: BlobEncoding::ZstdV1,
    encoded_size_bytes: compressed.len() as u64,
    expanded_size_bytes: 9,
    entry_count: 1,
  };
  assert!(matches!(
    limited.write_blob_if_absent(&too_large, reader(&compressed)).await,
    Err(CacheError::Limit(_))
  ));
}

#[tokio::test]
async fn quarantines_truncated_blobs_and_malformed_actions() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let bytes = b"bundle";
  let descriptor = blob(bytes);
  store.write_blob_if_absent(&descriptor, reader(bytes)).await.unwrap();
  let blob_path = store.blob_path(&descriptor);
  fs::write(&blob_path, b"x").unwrap();
  assert!(matches!(
    store.read_blob(&descriptor).await,
    Err(CacheError::Corrupt { .. })
  ));
  assert!(!blob_path.exists());

  let logical = result(action(2), None, "value");
  store.write_action_if_absent("ns", &logical).await.unwrap();
  let action_path = store.action_path("ns", &logical.action).unwrap();
  fs::write(&action_path, b"not-json").unwrap();
  assert!(matches!(
    store.get_action("ns", &logical.action).await,
    Err(CacheError::Corrupt { .. })
  ));
  assert!(!action_path.exists());
  assert!(fs::read_dir(store.layout.join("quarantine")).unwrap().count() >= 4);
}

#[tokio::test]
async fn quarantines_semantically_invalid_and_misbound_actions() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();

  let invalid = result(action(5), None, "invalid version");
  let invalid_path = store.action_path("ns", &invalid.action).unwrap();
  fs::create_dir_all(invalid_path.parent().unwrap()).unwrap();
  let mut invalid_json = serde_json::to_value(&invalid).unwrap();
  invalid_json["result_version"] = json!(2);
  fs::write(&invalid_path, serde_json::to_vec(&invalid_json).unwrap()).unwrap();
  assert!(matches!(
    store.get_action("ns", &invalid.action).await,
    Err(CacheError::Corrupt { .. })
  ));

  let requested = action(6);
  let misbound = result(action(7), None, "wrong action");
  let misbound_path = store.action_path("ns", &requested).unwrap();
  fs::create_dir_all(misbound_path.parent().unwrap()).unwrap();
  fs::write(&misbound_path, serde_json::to_vec(&misbound).unwrap()).unwrap();
  assert!(matches!(
    store.get_action("ns", &requested).await,
    Err(CacheError::Corrupt { .. })
  ));
}

#[tokio::test]
async fn rejects_oversized_action_metadata_before_reading_it() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let key = action(12);
  let path = store.action_path("ns", &key).unwrap();
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::File::create(&path)
    .unwrap()
    .set_len(octa_cache_protocol::MAX_ACTION_RESULT_WIRE_BYTES as u64 + 1)
    .unwrap();

  assert!(matches!(
    store.get_action("ns", &key).await,
    Err(CacheError::Corrupt { reason, .. }) if reason.contains("exceeds")
  ));
  assert!(!path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn local_objects_never_follow_symbolic_links() {
  use std::os::unix::fs::symlink;

  let root = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();

  let key = action(13);
  let action_path = store.action_path("ns", &key).unwrap();
  fs::create_dir_all(action_path.parent().unwrap()).unwrap();
  fs::write(
    outside.path().join("action.json"),
    serde_json::to_vec(&result(key, None, "outside")).unwrap(),
  )
  .unwrap();
  symlink(outside.path().join("action.json"), &action_path).unwrap();
  assert!(matches!(
    store.get_action("ns", &key).await,
    Err(CacheError::Corrupt { .. })
  ));

  let descriptor = blob(b"outside blob");
  let blob_path = store.blob_path(&descriptor);
  fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
  fs::write(outside.path().join("blob"), b"outside blob").unwrap();
  symlink(outside.path().join("blob"), &blob_path).unwrap();
  assert_eq!(
    store
      .find_missing_blobs(std::slice::from_ref(&descriptor))
      .await
      .unwrap(),
    vec![descriptor]
  );
  assert!(!blob_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn local_publication_never_follows_a_symbolic_link_ancestor() {
  use std::os::unix::fs::symlink;

  let root = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let bytes = b"ancestor escape";
  let descriptor = blob(bytes);
  let destination = store.blob_path(&descriptor);
  let shard = destination.parent().unwrap();
  symlink(outside.path(), shard).unwrap();

  assert!(matches!(
    store.write_blob_if_absent(&descriptor, reader(bytes)).await,
    Err(CacheError::Corrupt { .. })
  ));
  assert!(!outside.path().join(destination.file_name().unwrap()).exists());
  assert_eq!(fs::read_dir(store.layout.join("tmp")).unwrap().count(), 0);
}

#[tokio::test]
async fn missing_lookup_quarantines_a_wrong_sized_blob() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let descriptor = blob(b"complete");
  let path = store.blob_path(&descriptor);
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::write(&path, b"short").unwrap();

  assert_eq!(
    store
      .find_missing_blobs(std::slice::from_ref(&descriptor))
      .await
      .unwrap(),
    vec![descriptor]
  );
  assert!(!path.exists());
}

#[tokio::test]
async fn rejects_wrong_blob_content_and_actions_with_missing_blobs() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let descriptor = blob(b"good");
  assert!(matches!(
    store.write_blob_if_absent(&descriptor, reader(b"evil")).await,
    Err(CacheError::InvalidBundle(_))
  ));
  assert!(!store.blob_path(&descriptor).exists());

  let logical = result(action(8), Some(descriptor), "unpublished");
  assert!(matches!(
    store.write_action_if_absent("ns", &logical).await,
    Err(CacheError::Corrupt { .. })
  ));
  assert!(!store.action_path("ns", &logical.action).unwrap().exists());

  let descriptor = blob(b"complete");
  let path = store.blob_path(&descriptor);
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::write(&path, b"short").unwrap();
  let incomplete = result(action(9), Some(descriptor), "incomplete");
  assert!(matches!(
    store.write_action_if_absent("ns", &incomplete).await,
    Err(CacheError::Corrupt { .. })
  ));
}

#[tokio::test]
async fn replaces_a_corrupt_resident_blob_with_the_verified_publication() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let expected = b"good";
  let descriptor = blob(expected);
  let path = store.blob_path(&descriptor);
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::write(&path, b"evil").unwrap();

  assert_eq!(
    store.write_blob_if_absent(&descriptor, reader(expected)).await.unwrap(),
    WriteOutcome::Written
  );
  let mut stored = store.read_blob(&descriptor).await.unwrap();
  let mut actual = Vec::new();
  stored.read_to_end(&mut actual).await.unwrap();
  assert_eq!(actual, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_publish_one_complete_blob() {
  let root = tempfile::tempdir().unwrap();
  let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap());
  let bytes = b"one immutable object".to_vec();
  let descriptor = blob(&bytes);
  let mut tasks = Vec::new();
  for _ in 0..16 {
    let store = Arc::clone(&store);
    let bytes = bytes.clone();
    let descriptor = descriptor.clone();
    tasks.push(tokio::spawn(async move {
      store
        .write_blob_if_absent(&descriptor, Box::pin(Cursor::new(bytes)))
        .await
        .unwrap()
    }));
  }
  let mut written = 0;
  for task in tasks {
    written += usize::from(task.await.unwrap() == WriteOutcome::Written);
  }
  assert_eq!(written, 1);
  let mut stored = store.read_blob(&descriptor).await.unwrap();
  let mut actual = Vec::new();
  stored.read_to_end(&mut actual).await.unwrap();
  assert_eq!(actual, bytes);
}

#[test]
fn process_writer_child() {
  let Ok(root) = std::env::var(CHILD_ROOT) else {
    return;
  };
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();
  runtime.block_on(async {
    let store = LocalCacheStore::open(LocalCacheConfig::new(root)).unwrap();
    let bytes = process_blob_bytes();
    store.write_blob_if_absent(&blob(&bytes), reader(&bytes)).await.unwrap();
  });
}

#[tokio::test]
async fn multiple_processes_never_observe_a_partial_blob() {
  let root = tempfile::tempdir().unwrap();
  let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
  let bytes = process_blob_bytes();
  let descriptor = blob(&bytes);
  let executable = std::env::current_exe().unwrap();
  let mut children = (0..4)
    .map(|_| {
      Command::new(&executable)
        .args(["--exact", "local::tests::process_writer_child"])
        .env(CHILD_ROOT, root.path())
        .spawn()
        .unwrap()
    })
    .collect::<Vec<_>>();

  loop {
    match store.read_blob(&descriptor).await {
      Ok(mut stored) => {
        let mut actual = Vec::new();
        stored.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, bytes);
      },
      Err(CacheError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {},
      Err(error) => panic!("reader observed an invalid publication state: {error}"),
    }
    if children.iter_mut().all(|child| child.try_wait().unwrap().is_some()) {
      break;
    }
    tokio::task::yield_now().await;
  }
  let mut stored = store.read_blob(&descriptor).await.unwrap();
  let mut actual = Vec::new();
  stored.read_to_end(&mut actual).await.unwrap();
  assert_eq!(actual, bytes);
}

#[tokio::test]
async fn garbage_collection_expires_old_actions_then_unreferenced_blobs() {
  let root = tempfile::tempdir().unwrap();
  let mut initial = LocalCacheConfig::new(root.path());
  initial.max_bytes = 1024 * 1024;
  initial.high_watermark_bytes = initial.max_bytes;
  initial.low_watermark_bytes = initial.max_bytes;
  initial.temporary_grace = Duration::from_millis(1);
  let store = LocalCacheStore::open(initial).unwrap();

  let first_blob = blob(b"first bundle payload");
  store
    .write_blob_if_absent(&first_blob, reader(b"first bundle payload"))
    .await
    .unwrap();
  let first = result(action(3), Some(first_blob.clone()), "old");
  store.write_action_if_absent("ns", &first).await.unwrap();
  store.get_action("ns", &first.action).await.unwrap();
  tokio::time::sleep(Duration::from_millis(10)).await;

  let second_blob = blob(b"second bundle payload");
  store
    .write_blob_if_absent(&second_blob, reader(b"second bundle payload"))
    .await
    .unwrap();
  let second = result(action(4), Some(second_blob.clone()), "new");
  store.write_action_if_absent("ns", &second).await.unwrap();
  store.get_action("ns", &second.action).await.unwrap();
  tokio::time::sleep(Duration::from_millis(10)).await;

  let total = gc::directory_bytes(&store.layout, store.config.max_entries).unwrap();
  let reclaimed = fs::metadata(store.action_path("ns", &first.action).unwrap())
    .unwrap()
    .len()
    + fs::metadata(store.blob_path(&first_blob)).unwrap().len();
  let mut constrained = store.config.clone();
  constrained.low_watermark_bytes = total.saturating_sub(reclaimed);
  constrained.high_watermark_bytes = constrained.low_watermark_bytes;
  let constrained = LocalCacheStore::open(constrained).unwrap();
  let report = constrained.prune().await.unwrap();

  assert_eq!(report.actions_removed, 1);
  assert_eq!(report.blobs_removed, 1);
  assert!(constrained.get_action("ns", &first.action).await.unwrap().is_none());
  assert_eq!(
    constrained
      .get_action("ns", &second.action)
      .await
      .unwrap()
      .map(|lookup| lookup.result),
    Some(second)
  );
}
