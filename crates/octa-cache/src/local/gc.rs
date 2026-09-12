//! Local capacity accounting and mark-and-sweep collection.
//!
//! Collection runs while holding the store-wide exclusive GC lock, so readers
//! and publishers cannot observe an object disappearing between validation and
//! use. A pass first removes aged maintenance files, then evicts action records
//! in least-recently-used order until projected usage reaches the low
//! watermark. Blob reference counts are adjusted as actions are selected, and
//! the sweep removes only blobs no surviving action can reach. The grace period
//! protects a blob that has been published but whose action record is not yet
//! visible.

use std::{
  collections::{HashMap, HashSet},
  fs, io,
  path::{Path, PathBuf},
  time::{Duration, SystemTime},
};

use octa_cache_protocol::{ActionResultV1, MAX_ACTION_RESULT_WIRE_BYTES};
use uuid::Uuid;

use super::{acquire_lock, blob_relative_path, LocalCacheStore, ACTION_EXTENSION};
use crate::{error::io_error, platform::sync_directory, CacheError, CacheResult};

/// Summary of one local garbage-collection pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GarbageCollection {
  /// Accounted bytes before collection.
  pub bytes_before: u64,
  /// Accounted bytes remaining after collection.
  pub bytes_after: u64,
  /// Expired action records removed.
  pub actions_removed: u64,
  /// Unreferenced immutable blobs removed.
  pub blobs_removed: u64,
  /// Abandoned temporary or quarantined files removed.
  pub maintenance_files_removed: u64,
}

/// Valid action metadata together with the sampled access time used for LRU.
struct ActionEntry {
  path: PathBuf,
  access: SystemTime,
  result: ActionResultV1,
}

pub(crate) async fn collect(store: LocalCacheStore, force: bool) -> CacheResult<GarbageCollection> {
  let _lock = acquire_lock(store.gc_lock_path(), true).await?;
  let capacity = store.capacity.clone();
  let report = tokio::task::spawn_blocking(move || collect_locked(&store, force))
    .await
    .map_err(CacheError::Worker)??;
  capacity.reconcile(report.bytes_after)?;
  Ok(report)
}

fn collect_locked(store: &LocalCacheStore, force: bool) -> CacheResult<GarbageCollection> {
  let mut report = GarbageCollection {
    bytes_before: directory_bytes(&store.layout, store.config.max_entries)?,
    ..GarbageCollection::default()
  };
  let now = SystemTime::now();
  cleanup_aged_files(
    &store.layout.join("tmp"),
    now,
    store.config.temporary_grace,
    store.config.max_entries,
    &mut report,
  )?;
  cleanup_aged_files(
    &store.layout.join("quarantine"),
    now,
    store.config.temporary_grace,
    store.config.max_entries,
    &mut report,
  )?;
  cleanup_orphan_access_markers(store, now, &mut report)?;

  let after_maintenance = directory_bytes(&store.layout, store.config.max_entries)?;
  let threshold = if force {
    store.config.low_watermark_bytes
  } else {
    store.config.high_watermark_bytes
  };
  if after_maintenance <= threshold {
    report.bytes_after = after_maintenance;
    return Ok(report);
  }

  // Plan action eviction before deleting anything. `projected` includes the
  // action record, its access marker, and a blob only when selecting this
  // action removes the final reference to that sufficiently old blob.
  let mut actions = load_actions(store)?;
  actions.sort_by_key(|entry| entry.access);
  let mut projected = after_maintenance;
  let mut planned_actions = HashSet::new();
  let mut references = HashMap::<PathBuf, u64>::new();
  for action in &actions {
    if let Some(blob) = &action.result.output_bundle {
      *references.entry(blob_relative_path(blob)).or_default() += 1;
    }
  }
  for action in &actions {
    if projected <= store.config.low_watermark_bytes {
      break;
    }
    projected = projected.saturating_sub(file_size(&action.path)?);
    let marker = action.path.with_extension("access");
    projected = projected.saturating_sub(file_size_if_present(&marker)?);
    if let Some(blob) = &action.result.output_bundle {
      let relative = blob_relative_path(blob);
      let remaining = references
        .get_mut(&relative)
        .expect("every action blob was counted before eviction");
      *remaining -= 1;
      let path = store.layout.join(&relative);
      if *remaining == 0 && path_exists(&path)? && older_than(&path, now, store.config.temporary_grace)? {
        projected = projected.saturating_sub(file_size(&path)?);
      }
    }
    planned_actions.insert(action.path.clone());
  }

  let mut removed_actions = HashSet::new();
  for path in &planned_actions {
    if remove_file_retry_later(path)? {
      sync_directory(path.parent().expect("an action record has a parent"))?;
      report.actions_removed += 1;
      removed_actions.insert(path.clone());
      let _ = remove_file_retry_later(&path.with_extension("access"));
    }
  }

  // Recompute reachability from records that actually survived action
  // deletion. Scanning the blob tree afterwards also collects objects orphaned
  // by an interrupted publisher or by previously corrupt metadata.
  let reachable = reachable_blobs(&actions, &removed_actions);
  for blob in regular_files(&store.layout.join("blobs"), store.config.max_entries)? {
    let relative = blob
      .strip_prefix(&store.layout)
      .map_err(|error| CacheError::Metadata(error.to_string()))?;
    if reachable.contains(relative) || !older_than(&blob, now, store.config.temporary_grace)? {
      continue;
    }
    if remove_file_retry_later(&blob)? {
      report.blobs_removed += 1;
    }
  }
  remove_empty_children(&store.layout.join("blobs"), store.config.max_entries)?;
  remove_empty_children(&store.layout.join("actions"), store.config.max_entries)?;
  report.bytes_after = directory_bytes(&store.layout, store.config.max_entries)?;
  Ok(report)
}

/// Computes reachability from action files that are no longer visible only.
///
/// In particular, the eviction plan is insufficient here: Windows may defer
/// deletion of an open action file. Its blob must remain reachable until a
/// later pass actually removes that action.
fn reachable_blobs(actions: &[ActionEntry], removed_actions: &HashSet<PathBuf>) -> HashSet<PathBuf> {
  actions
    .iter()
    .filter(|entry| !removed_actions.contains(&entry.path))
    .filter_map(|entry| entry.result.output_bundle.as_ref())
    .map(blob_relative_path)
    .collect()
}

fn load_actions(store: &LocalCacheStore) -> CacheResult<Vec<ActionEntry>> {
  let mut actions = Vec::new();
  for path in regular_files(&store.layout.join("actions"), store.config.max_entries)? {
    if path.extension().and_then(|extension| extension.to_str()) != Some(ACTION_EXTENSION) {
      continue;
    }
    if file_size(&path)? > MAX_ACTION_RESULT_WIRE_BYTES as u64 {
      quarantine_action(store, &path, "action metadata exceeds its wire-size limit")?;
      continue;
    }
    let bytes = fs::read(&path).map_err(|error| io_error("read action during garbage collection", &path, error))?;
    let result = match serde_json::from_slice::<ActionResultV1>(&bytes) {
      Ok(result) => result,
      Err(error) => {
        quarantine_action(store, &path, &format!("action metadata cannot be decoded: {error}"))?;
        continue;
      },
    };
    if let Err(error) = result.validate() {
      quarantine_action(store, &path, &error.to_string())?;
      continue;
    }
    let expected_name = format!(
      "{}-{}.{}",
      result.action.hex(),
      result.action.size_bytes(),
      ACTION_EXTENSION
    );
    if path.file_name().and_then(|name| name.to_str()) != Some(&expected_name) {
      quarantine_action(store, &path, "action metadata is not stored under its bound digest")?;
      continue;
    }
    let access_path = path.with_extension("access");
    let access = fs::symlink_metadata(&access_path)
      .or_else(|_| fs::symlink_metadata(&path))
      .and_then(|metadata| metadata.modified())
      .map_err(|error| io_error("read action access time", &path, error))?;
    actions.push(ActionEntry { path, access, result });
  }
  Ok(actions)
}

/// Removes sampled access markers whose action record no longer exists.
///
/// A marker can survive an unclean shutdown between maintenance operations or
/// a platform-specific deferred deletion. The grace period prevents treating a
/// concurrently copied cache directory as immediately disposable state.
fn cleanup_orphan_access_markers(
  store: &LocalCacheStore,
  now: SystemTime,
  report: &mut GarbageCollection,
) -> CacheResult<()> {
  for marker in regular_files(&store.layout.join("actions"), store.config.max_entries)? {
    if marker.extension().and_then(|extension| extension.to_str()) != Some("access") {
      continue;
    }
    let action = marker.with_extension(ACTION_EXTENSION);
    if !path_exists(&action)?
      && older_than(&marker, now, store.config.temporary_grace)?
      && remove_file_retry_later(&marker)?
    {
      report.maintenance_files_removed += 1;
    }
  }
  Ok(())
}

fn quarantine_action(store: &LocalCacheStore, path: &Path, reason: &str) -> CacheResult<()> {
  let name = format!(
    "{}-{}",
    Uuid::new_v4(),
    path.file_name().and_then(|name| name.to_str()).unwrap_or("action")
  );
  let destination = store.layout.join("quarantine").join(name);
  fs::rename(path, &destination)
    .map_err(|error| io_error("quarantine invalid action during collection", path, error))?;
  let reason_path = destination.with_extension("reason");
  fs::write(&reason_path, reason)
    .map_err(|error| io_error("write garbage-collection quarantine reason", reason_path, error))?;
  sync_directory(path.parent().expect("an action record has a parent"))?;
  sync_directory(destination.parent().expect("a quarantine object has a parent"))?;
  let _ = remove_file_retry_later(&path.with_extension("access"));
  Ok(())
}

fn cleanup_aged_files(
  root: &Path,
  now: SystemTime,
  grace: Duration,
  max_entries: usize,
  report: &mut GarbageCollection,
) -> CacheResult<()> {
  for path in regular_files(root, max_entries)? {
    if older_than(&path, now, grace)? && remove_file_retry_later(&path)? {
      report.maintenance_files_removed += 1;
    }
  }
  remove_empty_children(root, max_entries)?;
  Ok(())
}

fn regular_files(root: &Path, max_entries: usize) -> CacheResult<Vec<PathBuf>> {
  let mut result = Vec::new();
  visit_regular_files(root, max_entries, |path| {
    result.push(path);
    Ok(())
  })?;
  Ok(result)
}

/// Iteratively visits regular files without following symbolic links.
fn visit_regular_files(
  root: &Path,
  max_entries: usize,
  mut visit: impl FnMut(PathBuf) -> CacheResult<()>,
) -> CacheResult<()> {
  let mut pending = vec![root.to_path_buf()];
  let mut visited = 0_usize;
  while let Some(directory) = pending.pop() {
    let entries = match fs::read_dir(&directory) {
      Ok(entries) => entries,
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => return Err(io_error("scan local cache directory", directory, error)),
    };
    for entry in entries {
      if visited >= max_entries {
        return Err(CacheError::Limit(format!(
          "local cache maintenance exceeds {max_entries} filesystem entries"
        )));
      }
      visited += 1;
      let entry = entry.map_err(|error| io_error("read local cache entry", &directory, error))?;
      let file_type = entry
        .file_type()
        .map_err(|error| io_error("inspect local cache entry", entry.path(), error))?;
      if file_type.is_dir() {
        pending.push(entry.path());
      } else if file_type.is_file() {
        visit(entry.path())?;
      }
    }
  }
  Ok(())
}

pub(super) fn directory_bytes(root: &Path, max_entries: usize) -> CacheResult<u64> {
  let mut total = 0_u64;
  visit_regular_files(root, max_entries, |path| {
    total = total.saturating_add(file_size(&path)?);
    Ok(())
  })?;
  Ok(total)
}

fn file_size(path: &Path) -> CacheResult<u64> {
  fs::symlink_metadata(path)
    .map(|metadata| metadata.len())
    .map_err(|error| io_error("measure local cache file", path, error))
}

fn file_size_if_present(path: &Path) -> CacheResult<u64> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(metadata.len()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
    Err(error) => Err(io_error("measure local cache file", path, error)),
  }
}

fn path_exists(path: &Path) -> CacheResult<bool> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(metadata.is_file()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(io_error("inspect local cache file", path, error)),
  }
}

fn older_than(path: &Path, now: SystemTime, age: Duration) -> CacheResult<bool> {
  let modified = fs::symlink_metadata(path)
    .and_then(|metadata| metadata.modified())
    .map_err(|error| io_error("read local cache file age", path, error))?;
  Ok(now.duration_since(modified).unwrap_or_default() >= age)
}

/// Windows may refuse to unlink a file that another process opened before it
/// acquired the GC lock. That object is safe to retain until the next pass.
fn remove_file_retry_later(path: &Path) -> CacheResult<bool> {
  match fs::remove_file(path) {
    Ok(()) => Ok(true),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error)
      if matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock
      ) =>
    {
      Ok(false)
    },
    Err(error) => Err(io_error("remove local cache file", path, error)),
  }
}

/// Removes empty descendants while preserving the fixed layout root itself.
fn remove_empty_children(root: &Path, max_entries: usize) -> CacheResult<()> {
  // Discover iteratively and remove in postorder. Cache contents may be
  // malformed, so maintenance must not recurse on attacker-controlled depth.
  let mut pending = vec![root.to_path_buf()];
  let mut directories = Vec::new();
  let mut visited = 0_usize;
  while let Some(directory) = pending.pop() {
    let entries = match fs::read_dir(&directory) {
      Ok(entries) => entries,
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => return Err(io_error("scan local cache directory", directory, error)),
    };
    for entry in entries {
      if visited >= max_entries {
        return Err(CacheError::Limit(format!(
          "local cache maintenance exceeds {max_entries} filesystem entries"
        )));
      }
      visited += 1;
      let entry = entry.map_err(|error| io_error("read local cache entry", &directory, error))?;
      if entry
        .file_type()
        .map_err(|error| io_error("inspect local cache entry", entry.path(), error))?
        .is_dir()
      {
        pending.push(entry.path());
        directories.push(entry.path());
      }
    }
  }
  for directory in directories.into_iter().rev() {
    match fs::remove_dir(&directory) {
      Ok(()) => {},
      Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) => {},
      Err(error) => return Err(io_error("remove empty local cache directory", directory, error)),
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use octa_cache_protocol::{ActionResultV1, BlobDescriptor, BlobEncoding, Digest, DigestAlgorithm};

  use super::*;
  use crate::{store::CacheStore, LocalCacheConfig};

  fn digest(byte: u8, size: u64) -> Digest {
    Digest::new(DigestAlgorithm::Blake3, [byte; 32], size)
  }

  fn action(action: Digest, output_bundle: Option<BlobDescriptor>) -> ActionResultV1 {
    ActionResultV1 {
      result_version: 1,
      action,
      output_bundle,
      stdout: None,
      task_outputs: BTreeMap::new(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    }
  }

  #[tokio::test]
  async fn collection_below_the_high_watermark_is_a_noop() {
    let root = tempfile::tempdir().unwrap();
    let store = LocalCacheStore::open(LocalCacheConfig::new(root.path())).unwrap();
    let report = collect(store, false).await.unwrap();
    assert_eq!(report.bytes_before, report.bytes_after);
    assert_eq!(report.actions_removed, 0);
    assert_eq!(report.blobs_removed, 0);
  }

  #[tokio::test]
  async fn collection_quarantines_invalid_action_records() {
    let root = tempfile::tempdir().unwrap();
    let mut config = LocalCacheConfig::new(root.path());
    config.high_watermark_bytes = 0;
    config.low_watermark_bytes = 0;
    let store = LocalCacheStore::open(config).unwrap();
    let action_dir = store.layout.join("actions/namespace");
    fs::create_dir_all(&action_dir).unwrap();
    fs::write(action_dir.join("invalid.json"), b"not-json").unwrap();
    let semantic = action(digest(1, 10), None);
    let mut semantic = serde_json::to_value(semantic).unwrap();
    semantic["result_version"] = serde_json::json!(2);
    fs::write(action_dir.join("semantic.json"), serde_json::to_vec(&semantic).unwrap()).unwrap();
    fs::write(action_dir.join("ignored.txt"), b"not an action record").unwrap();

    collect(store.clone(), false).await.unwrap();
    assert!(!action_dir.join("invalid.json").exists());
    assert!(!action_dir.join("semantic.json").exists());
    assert!(action_dir.join("ignored.txt").exists());
    assert!(
      regular_files(&store.layout.join("quarantine"), usize::MAX)
        .unwrap()
        .len()
        >= 4
    );
  }

  #[tokio::test]
  async fn collection_retains_a_blob_referenced_by_a_newer_action() {
    let root = tempfile::tempdir().unwrap();
    let mut config = LocalCacheConfig::new(root.path());
    config.max_bytes = 1024 * 1024;
    config.high_watermark_bytes = config.max_bytes;
    config.low_watermark_bytes = config.max_bytes;
    config.temporary_grace = Duration::from_millis(1);
    let store = LocalCacheStore::open(config).unwrap();
    let bytes = b"shared immutable blob";
    let descriptor = BlobDescriptor {
      digest: Digest::new(
        DigestAlgorithm::Blake3,
        *blake3::hash(bytes).as_bytes(),
        bytes.len() as u64,
      ),
      encoding: BlobEncoding::Identity,
      encoded_size_bytes: bytes.len() as u64,
      expanded_size_bytes: bytes.len() as u64,
      entry_count: 1,
    };
    store
      .write_blob_if_absent(&descriptor, Box::pin(std::io::Cursor::new(bytes)))
      .await
      .unwrap();
    std::thread::sleep(Duration::from_millis(5));

    let old = action(digest(2, 10), Some(descriptor.clone()));
    store.write_action_if_absent("ns", &old).await.unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let new = action(digest(3, 10), Some(descriptor.clone()));
    store.write_action_if_absent("ns", &new).await.unwrap();

    let total = directory_bytes(&store.layout, store.config.max_entries).unwrap();
    let old_bytes = file_size(&store.action_path("ns", &old.action).unwrap()).unwrap();
    let mut constrained = store.config.clone();
    constrained.low_watermark_bytes = total - old_bytes;
    constrained.high_watermark_bytes = constrained.low_watermark_bytes;
    let constrained = LocalCacheStore::open(constrained).unwrap();
    let report = collect(constrained.clone(), false).await.unwrap();

    assert_eq!(report.actions_removed, 1);
    assert_eq!(report.blobs_removed, 0);
    assert!(constrained.blob_path(&descriptor).exists());
    assert!(constrained.get_action("ns", &old.action).await.unwrap().is_none());
    assert_eq!(
      constrained
        .get_action("ns", &new.action)
        .await
        .unwrap()
        .map(|lookup| lookup.result),
      Some(new)
    );
  }

  #[test]
  fn deferred_action_deletion_keeps_its_blob_reachable() {
    let shared = BlobDescriptor {
      digest: digest(9, 10),
      encoding: BlobEncoding::Identity,
      encoded_size_bytes: 10,
      expanded_size_bytes: 10,
      entry_count: 1,
    };
    let retained_path = PathBuf::from("retained.json");
    let removed_path = PathBuf::from("removed.json");
    let actions = vec![
      ActionEntry {
        path: retained_path.clone(),
        access: SystemTime::UNIX_EPOCH,
        result: action(digest(1, 1), Some(shared.clone())),
      },
      ActionEntry {
        path: removed_path.clone(),
        access: SystemTime::UNIX_EPOCH,
        result: action(digest(2, 1), Some(shared.clone())),
      },
    ];

    let reachable = reachable_blobs(&actions, &HashSet::from([removed_path]));
    assert_eq!(reachable, HashSet::from([blob_relative_path(&shared)]));
    assert!(reachable_blobs(&actions, &HashSet::from([retained_path, PathBuf::from("removed.json")])).is_empty());
  }

  #[test]
  fn collection_removes_only_aged_orphan_access_markers() {
    let root = tempfile::tempdir().unwrap();
    let mut config = LocalCacheConfig::new(root.path());
    config.temporary_grace = Duration::from_millis(500);
    let store = LocalCacheStore::open(config).unwrap();
    let action_dir = store.layout.join("actions/namespace/00");
    fs::create_dir_all(&action_dir).unwrap();
    let aged = action_dir.join("aged.access");
    fs::write(&aged, []).unwrap();
    let live = action_dir.join("live.access");
    fs::write(live.with_extension(ACTION_EXTENSION), b"action exists").unwrap();
    fs::write(&live, []).unwrap();
    let mut report = GarbageCollection::default();

    cleanup_orphan_access_markers(&store, SystemTime::now() + Duration::from_secs(1), &mut report).unwrap();

    assert_eq!(report.maintenance_files_removed, 1);
    assert!(!aged.exists());
    assert!(live.exists());
  }

  #[test]
  fn maintenance_removes_only_aged_files_and_empty_directories() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested/deeper");
    fs::create_dir_all(&nested).unwrap();
    let aged = nested.join("old.tmp");
    fs::write(&aged, b"old").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let fresh = nested.join("fresh.tmp");
    fs::write(&fresh, b"fresh").unwrap();
    let collection_time = fs::metadata(&fresh).unwrap().modified().unwrap();
    let mut report = GarbageCollection::default();
    cleanup_aged_files(
      root.path(),
      collection_time,
      Duration::from_millis(2),
      usize::MAX,
      &mut report,
    )
    .unwrap();
    assert_eq!(report.maintenance_files_removed, 1);
    assert!(!aged.exists());
    assert!(fresh.exists());
    fs::remove_file(fresh).unwrap();
    remove_empty_children(root.path(), usize::MAX).unwrap();
    assert!(!root.path().join("nested").exists());
  }

  #[test]
  fn absent_paths_are_safe_during_maintenance() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent");
    assert!(regular_files(&absent, usize::MAX).unwrap().is_empty());
    assert_eq!(file_size_if_present(&absent).unwrap(), 0);
    assert!(!path_exists(&absent).unwrap());
    assert!(!remove_file_retry_later(&absent).unwrap());
    remove_empty_children(&absent, usize::MAX).unwrap();
    assert!(!absent.exists());
  }

  #[test]
  fn maintenance_traversals_enforce_the_configured_entry_limit() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("one"), []).unwrap();
    fs::write(root.path().join("two"), []).unwrap();

    assert!(matches!(regular_files(root.path(), 1), Err(CacheError::Limit(_))));
    assert!(matches!(
      remove_empty_children(root.path(), 1),
      Err(CacheError::Limit(_))
    ));
  }

  #[cfg(unix)]
  #[test]
  fn filesystem_permission_failures_are_reported_or_retried() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let protected = root.path().join("protected");
    fs::create_dir(&protected).unwrap();
    fs::write(protected.join("entry"), b"data").unwrap();
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(matches!(
      regular_files(&protected, usize::MAX),
      Err(CacheError::Io { .. })
    ));
    assert!(matches!(
      file_size_if_present(&protected.join("entry")),
      Err(CacheError::Io { .. })
    ));
    assert!(matches!(
      path_exists(&protected.join("entry")),
      Err(CacheError::Io { .. })
    ));
    assert!(matches!(
      remove_empty_children(&protected, usize::MAX),
      Err(CacheError::Io { .. })
    ));
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(!remove_file_retry_later(&protected.join("entry")).unwrap());

    fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(protected.join("entry")).unwrap();
    let empty = protected.join("empty");
    fs::create_dir(&empty).unwrap();
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(matches!(
      remove_empty_children(&protected, usize::MAX),
      Err(CacheError::Io { .. })
    ));
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
  }
}
