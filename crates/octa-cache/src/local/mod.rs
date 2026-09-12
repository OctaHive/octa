//! Concurrent local action cache and content-addressed blob store.
//!
//! Objects are first written and synchronized under `tmp`, then atomically
//! renamed while holding a per-object cross-process lock. The final path is
//! therefore either absent or complete. A separate shared/exclusive GC lock
//! prevents collection between the existence checks and publication steps.

mod capacity;
mod gc;
mod object;

use std::{
  fs, io,
  path::{Path, PathBuf},
  time::Duration,
};

use async_trait::async_trait;
use capacity::CapacityLedger;
use fs2::FileExt as _;
pub(crate) use object::{acquire_lock, blob_relative_path};
use object::{open_lock_file, record_sampled_access, verify_encoded_blob, LockedBlobReader};
use octa_cache_protocol::{ActionResultV1, BlobDescriptor, Digest, DigestAlgorithm, MAX_ACTION_RESULT_WIRE_BYTES};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use uuid::Uuid;

use crate::{
  error::io_error,
  locking::shard_name,
  platform::{is_link_or_reparse, sync_directory},
  store::{validate_namespace, ActionLookup, BlobReader, CacheStore, WriteOutcome},
  CacheError, CacheResult,
};

pub use gc::GarbageCollection;

const LAYOUT_VERSION: &str = "v1";
const ACTION_EXTENSION: &str = "json";
const BLOB_EXTENSION: &str = "blob";
const GIBIBYTE: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_BYTES: u64 = 20 * GIBIBYTE;
const DEFAULT_HIGH_WATERMARK_BYTES: u64 = 18 * GIBIBYTE;
const DEFAULT_LOW_WATERMARK_BYTES: u64 = 16 * GIBIBYTE;
const DEFAULT_MAX_EXPANDED_BLOB_BYTES: u64 = 100 * GIBIBYTE;
const DEFAULT_MAX_BLOB_COMPRESSION_RATIO: u64 = 1_000;
const DEFAULT_MAX_ENTRIES: usize = 1_000_000;
const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Capacity and maintenance policy for one local cache directory.
#[derive(Clone, Debug)]
pub struct LocalCacheConfig {
  /// Parent directory under which the versioned layout is created.
  pub root: PathBuf,
  /// Hard limit for a single object and best-effort limit for total stored bytes.
  pub max_bytes: u64,
  /// Maximum canonical bytes accepted while verifying one encoded blob.
  pub max_expanded_blob_bytes: u64,
  /// Maximum expanded-to-encoded ratio accepted during blob publication.
  pub max_blob_compression_ratio: u64,
  /// Maximum filesystem entries visited by one maintenance scan.
  pub max_entries: usize,
  /// Usage that triggers collection before a new blob is accepted.
  pub high_watermark_bytes: u64,
  /// Target usage after a collection pass.
  pub low_watermark_bytes: u64,
  /// Age during which temporary and newly unreferenced blobs survive collection.
  pub temporary_grace: Duration,
  /// Minimum interval between access-marker updates for one action.
  pub access_update_interval: Duration,
}

impl LocalCacheConfig {
  /// Creates a bounded default profile rooted at `root`.
  pub fn new(root: impl Into<PathBuf>) -> Self {
    Self {
      root: root.into(),
      max_bytes: DEFAULT_MAX_BYTES,
      max_expanded_blob_bytes: DEFAULT_MAX_EXPANDED_BLOB_BYTES,
      max_blob_compression_ratio: DEFAULT_MAX_BLOB_COMPRESSION_RATIO,
      max_entries: DEFAULT_MAX_ENTRIES,
      high_watermark_bytes: DEFAULT_HIGH_WATERMARK_BYTES,
      low_watermark_bytes: DEFAULT_LOW_WATERMARK_BYTES,
      temporary_grace: DEFAULT_MAINTENANCE_INTERVAL,
      access_update_interval: DEFAULT_MAINTENANCE_INTERVAL,
    }
  }

  fn validate(&self) -> CacheResult<()> {
    if !self.root.is_absolute() {
      return Err(CacheError::Configuration(
        "local cache root must be an absolute operator-owned directory".to_owned(),
      ));
    }
    if self.max_bytes == 0
      || self.max_expanded_blob_bytes == 0
      || self.max_blob_compression_ratio == 0
      || self.max_entries == 0
      || self.low_watermark_bytes > self.high_watermark_bytes
      || self.high_watermark_bytes > self.max_bytes
    {
      return Err(CacheError::Configuration(
        "local cache sizes, compression ratio, and entry limit must be nonzero, with 0 <= low <= high <= maximum"
          .to_owned(),
      ));
    }
    if self.temporary_grace.is_zero() || self.access_update_interval.is_zero() {
      return Err(CacheError::Configuration(
        "local cache maintenance intervals must be greater than zero".to_owned(),
      ));
    }
    Ok(())
  }
}

/// Filesystem implementation of the shared immutable [`CacheStore`] boundary.
#[derive(Clone, Debug)]
pub struct LocalCacheStore {
  config: LocalCacheConfig,
  layout: PathBuf,
  capacity: CapacityLedger,
}

/// Current capacity view of an opened local cache.
///
/// Usage comes from the cross-process capacity ledger and is reconciled by GC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCacheStatus {
  /// Versioned directory containing action records, blobs, and maintenance state.
  pub layout: PathBuf,
  /// Accounted bytes currently held by the store.
  pub used_bytes: u64,
  /// Configured hard capacity for this store.
  pub max_bytes: u64,
  /// Usage at which opportunistic collection begins.
  pub high_watermark_bytes: u64,
  /// Target usage of a completed collection pass.
  pub low_watermark_bytes: u64,
}

impl LocalCacheStore {
  /// Opens or initializes a version-one local cache layout.
  pub fn open(config: LocalCacheConfig) -> CacheResult<Self> {
    config.validate()?;
    let layout = config.root.join(LAYOUT_VERSION);
    create_cache_root(&config.root)?;
    // Create and validate one level at a time. `create_dir_all` over the whole
    // path could follow a pre-existing directory symlink before we inspect it.
    for directory in [
      layout.clone(),
      layout.join("actions"),
      layout.join("blobs"),
      layout.join("blobs/blake3"),
      layout.join("tmp"),
      layout.join("locks"),
      layout.join("locks/objects"),
      layout.join("quarantine"),
      layout.join("restore-journal"),
    ] {
      create_cache_directory(&directory)?;
    }
    // Store startup follows the same GC -> capacity lock order as writers and
    // collection. This matters when a missing or torn ledger triggers a full
    // scan: no other process may publish or remove an object mid-measurement.
    let gc_lock_path = layout.join("locks/gc.lock");
    let gc_lock = open_lock_file(&gc_lock_path)?;
    gc_lock
      .lock_exclusive()
      .map_err(|error| io_error("lock local cache during startup", &gc_lock_path, error))?;
    let capacity = CapacityLedger::open(layout.clone(), config.max_entries)?;
    drop(gc_lock);
    Ok(Self {
      config,
      layout,
      capacity,
    })
  }

  /// Versioned directory containing this store's objects and maintenance state.
  pub fn layout_root(&self) -> &Path {
    &self.layout
  }

  /// Returns the capacity state used by management commands and diagnostics.
  pub async fn status(&self) -> CacheResult<LocalCacheStatus> {
    Ok(LocalCacheStatus {
      layout: self.layout.clone(),
      used_bytes: self.capacity.used().await?,
      max_bytes: self.config.max_bytes,
      high_watermark_bytes: self.config.high_watermark_bytes,
      low_watermark_bytes: self.config.low_watermark_bytes,
    })
  }

  /// Runs one exclusive mark-and-sweep pass when usage exceeds the high watermark.
  pub async fn prune(&self) -> CacheResult<GarbageCollection> {
    gc::collect(self.clone(), true).await
  }

  /// Moves a blob rejected by the bundle decoder out of the normal lookup tree.
  ///
  /// The caller supplies the integrity failure because only bundle extraction
  /// can verify canonical entry structure in addition to bytes and sizes.
  pub async fn quarantine_blob(&self, blob: &BlobDescriptor, reason: impl Into<String>) -> CacheResult<()> {
    blob.validate()?;
    let source = self.blob_path(blob);
    self.quarantine(source, reason.into()).await
  }

  async fn reserve_capacity(&self, incoming: u64) -> CacheResult<fs::File> {
    if incoming > self.config.max_bytes {
      return Err(CacheError::Limit(format!(
        "cache object of {incoming} bytes exceeds the local capacity of {} bytes",
        self.config.max_bytes
      )));
    }
    let gc_lock = acquire_lock(self.gc_lock_path(), false).await?;
    if self
      .capacity
      .try_reserve(incoming, self.config.high_watermark_bytes)
      .await?
    {
      return Ok(gc_lock);
    }
    drop(gc_lock);
    gc::collect(self.clone(), false).await?;
    let gc_lock = acquire_lock(self.gc_lock_path(), false).await?;
    if !self.capacity.try_reserve(incoming, self.config.max_bytes).await? {
      return Err(CacheError::Limit(
        "local cache remains full after garbage collection".to_owned(),
      ));
    }
    Ok(gc_lock)
  }

  fn validate_blob_resources(&self, blob: &BlobDescriptor) -> CacheResult<()> {
    let allowed_expanded = blob
      .encoded_size_bytes
      .saturating_mul(self.config.max_blob_compression_ratio);
    if blob.expanded_size_bytes > self.config.max_expanded_blob_bytes || blob.expanded_size_bytes > allowed_expanded {
      return Err(CacheError::Limit(
        "blob descriptor exceeds local expansion or compression-ratio limits".to_owned(),
      ));
    }
    Ok(())
  }

  fn action_path(&self, namespace: &str, action: &Digest) -> CacheResult<PathBuf> {
    validate_action_digest(action)?;
    let namespace = namespace_directory(namespace)?;
    let hash = action.hex();
    Ok(
      self
        .layout
        .join("actions")
        .join(namespace)
        .join(&hash[..2])
        .join(format!("{}-{}.{}", hash, action.size_bytes(), ACTION_EXTENSION)),
    )
  }

  fn blob_path(&self, blob: &BlobDescriptor) -> PathBuf {
    self.layout.join(blob_relative_path(blob))
  }

  fn object_lock_path(&self, key: &str) -> PathBuf {
    self
      .layout
      .join("locks/objects")
      .join(format!("{}.lock", shard_name(key.as_bytes())))
  }

  fn gc_lock_path(&self) -> PathBuf {
    self.layout.join("locks/gc.lock")
  }

  async fn quarantine(&self, source: PathBuf, reason: String) -> CacheResult<()> {
    let _gc = acquire_lock(self.gc_lock_path(), false).await?;
    let _object = acquire_lock(self.object_lock_path(&source.to_string_lossy()), true).await?;
    if !validate_cache_parent(&self.layout, &source, false)? {
      return Ok(());
    }
    match fs::symlink_metadata(&source) {
      Ok(_) => {},
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
      Err(error) => return Err(io_error("inspect cache object for quarantine", &source, error)),
    }
    let name = format!(
      "{}-{}",
      Uuid::new_v4(),
      source.file_name().and_then(|name| name.to_str()).unwrap_or("object")
    );
    let destination = self.layout.join("quarantine").join(name);
    fs::rename(&source, &destination).map_err(|error| io_error("quarantine cache object", &source, error))?;
    let reason_path = destination.with_extension("reason");
    fs::write(&reason_path, reason.as_bytes())
      .map_err(|error| io_error("write quarantine reason", &reason_path, error))?;
    if let Some(parent) = source.parent() {
      sync_directory(parent)?;
    }
    sync_directory(destination.parent().expect("quarantine object has a parent"))?;
    Ok(())
  }

  async fn quarantine_action(&self, path: PathBuf, reason: String) -> CacheError {
    match self.quarantine(path.clone(), reason.clone()).await {
      Ok(()) => CacheError::Corrupt { path, reason },
      Err(error) => error,
    }
  }

  async fn publish_bytes(&self, destination: &Path, bytes: &[u8]) -> CacheResult<WriteOutcome> {
    let temporary = self.temporary_path("metadata");
    let mut file = tokio::fs::OpenOptions::new()
      .create_new(true)
      .write(true)
      .open(&temporary)
      .await
      .map_err(|error| io_error("create temporary cache metadata", &temporary, error))?;
    if let Err(error) = async {
      file.write_all(bytes).await?;
      file.sync_all().await
    }
    .await
    {
      let _ = tokio::fs::remove_file(&temporary).await;
      return Err(io_error("write temporary cache metadata", &temporary, error));
    }
    let outcome = match self.publish_temporary(&temporary, destination).await {
      Ok(outcome) => outcome,
      Err(error) => {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
      },
    };
    if outcome == WriteOutcome::AlreadyPresent {
      let _ = tokio::fs::remove_file(&temporary).await;
    }
    Ok(outcome)
  }

  async fn publish_temporary(&self, temporary: &Path, destination: &Path) -> CacheResult<WriteOutcome> {
    let parent = destination
      .parent()
      .ok_or_else(|| CacheError::Configuration("cache object has no parent directory".to_owned()))?;
    validate_cache_parent(&self.layout, destination, true)?;
    // The per-object lock turns the final existence-check and rename into one
    // create-if-absent operation across processes. The caller has already
    // synchronized and, for blobs, verified the temporary file.
    let _object = acquire_lock(self.object_lock_path(&destination.to_string_lossy()), true).await?;
    match tokio::fs::symlink_metadata(destination).await {
      Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
        // The caller decides whether an existing immutable value is equal.
        // Retaining the synchronized temporary lets blob publication replace a
        // corrupt resident object without rereading the producer stream.
        return Ok(WriteOutcome::AlreadyPresent);
      },
      Ok(_) => {
        let _ = tokio::fs::remove_file(temporary).await;
        return Err(CacheError::Corrupt {
          path: destination.to_path_buf(),
          reason: "cache object path is not a regular file".to_owned(),
        });
      },
      Err(error) if error.kind() == io::ErrorKind::NotFound => {},
      Err(error) => return Err(io_error("inspect cache object before publication", destination, error)),
    }
    tokio::fs::rename(temporary, destination)
      .await
      .map_err(|error| io_error("publish immutable cache object", destination, error))?;
    sync_directory(parent)?;
    Ok(WriteOutcome::Written)
  }

  fn temporary_path(&self, kind: &str) -> PathBuf {
    self.layout.join("tmp").join(format!("{kind}-{}.tmp", Uuid::new_v4()))
  }

  async fn finish_reservation(&self, bytes: u64, result: CacheResult<WriteOutcome>) -> CacheResult<WriteOutcome> {
    if matches!(result, Ok(WriteOutcome::Written)) {
      return result;
    }
    match self.capacity.release(bytes).await {
      Ok(()) => result,
      Err(release) => match result {
        Ok(_) => Err(release),
        Err(primary) => Err(CacheError::CapacityRelease {
          primary: Box::new(primary),
          release: Box::new(release),
        }),
      },
    }
  }

  async fn existing_blob_is_valid(&self, blob: &BlobDescriptor) -> CacheResult<bool> {
    let path = self.blob_path(blob);
    if !validate_cache_parent(&self.layout, &path, false)? {
      return Ok(false);
    }
    let metadata = match tokio::fs::symlink_metadata(&path).await {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
      Err(error) => return Err(io_error("inspect existing local cache blob", &path, error)),
    };
    let valid_shape =
      metadata.file_type().is_file() && !metadata.file_type().is_symlink() && metadata.len() == blob.encoded_size_bytes;
    if valid_shape
      && verify_blob_file(path.clone(), blob.clone(), self.config.max_expanded_blob_bytes)
        .await
        .is_ok()
    {
      return Ok(true);
    }
    self
      .quarantine_blob(blob, "existing blob failed validation before publication")
      .await?;
    Ok(false)
  }

  async fn write_blob_reserved(
    &self,
    blob: &BlobDescriptor,
    mut body: BlobReader,
    gc_lock: fs::File,
  ) -> CacheResult<WriteOutcome> {
    let mut gc_lock = Some(gc_lock);
    let destination = self.blob_path(blob);
    let temporary = self.temporary_path("blob");
    let mut file = tokio::fs::OpenOptions::new()
      .create_new(true)
      .write(true)
      .open(&temporary)
      .await
      .map_err(|error| io_error("create temporary cache blob", &temporary, error))?;
    // Read one byte beyond the declared size so both truncation and surplus
    // producer data are rejected before the object becomes visible.
    let copied = match tokio::io::copy(
      &mut body.as_mut().take(blob.encoded_size_bytes.saturating_add(1)),
      &mut file,
    )
    .await
    {
      Ok(copied) => copied,
      Err(error) => {
        drop(file);
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(io_error("write temporary cache blob", &temporary, error));
      },
    };
    if copied != blob.encoded_size_bytes {
      drop(file);
      let _ = tokio::fs::remove_file(&temporary).await;
      return Err(CacheError::InvalidBundle(format!(
        "blob stream contained {copied} bytes, expected {}",
        blob.encoded_size_bytes
      )));
    }
    if let Err(error) = file.sync_all().await {
      drop(file);
      let _ = tokio::fs::remove_file(&temporary).await;
      return Err(io_error("synchronize temporary cache blob", &temporary, error));
    }
    drop(file);
    if let Err(error) = verify_blob_file(temporary.clone(), blob.clone(), self.config.max_expanded_blob_bytes).await {
      let _ = tokio::fs::remove_file(&temporary).await;
      return Err(error);
    }
    let outcome = match self.publish_temporary(&temporary, &destination).await {
      Ok(outcome) => outcome,
      Err(error) => {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
      },
    };
    match outcome {
      WriteOutcome::Written => Ok(WriteOutcome::Written),
      WriteOutcome::AlreadyPresent => {
        match verify_blob_file(destination.clone(), blob.clone(), self.config.max_expanded_blob_bytes).await {
          Ok(()) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Ok(WriteOutcome::AlreadyPresent)
          },
          Err(error) => {
            // Release the shared GC lock before quarantine reacquires the GC and
            // object-lock hierarchy. The verified temporary survives because it
            // is fresh and cannot be collected before the configured grace age.
            drop(gc_lock.take());
            self
              .quarantine_blob(blob, format!("existing blob failed publication verification: {error}"))
              .await?;
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(error)
          },
        }
      },
      WriteOutcome::Conflict => unreachable!("raw object publication does not compare values"),
    }
  }

  async fn write_action_reserved(
    &self,
    destination: &Path,
    result: &ActionResultV1,
    bytes: &[u8],
    _gc_lock: fs::File,
  ) -> CacheResult<WriteOutcome> {
    // Action metadata is the publication point for a cache hit. Requiring its
    // blob first prevents readers from observing a committed dangling result.
    if let Some(blob) = &result.output_bundle {
      let path = self.blob_path(blob);
      if !validate_cache_parent(&self.layout, &path, false)? {
        return Err(CacheError::Corrupt {
          path,
          reason: "action result references a missing blob".to_owned(),
        });
      }
      let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
          return Err(CacheError::Corrupt {
            path,
            reason: "action result references a missing blob".to_owned(),
          })
        },
        Err(error) => return Err(io_error("verify blob before action publication", &path, error)),
      };
      if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() != blob.encoded_size_bytes
      {
        return Err(CacheError::Corrupt {
          path,
          reason: "action result references an absent or incomplete blob".to_owned(),
        });
      }
    }
    match self.publish_bytes(destination, bytes).await? {
      WriteOutcome::Written => Ok(WriteOutcome::Written),
      WriteOutcome::AlreadyPresent => {
        let metadata = tokio::fs::symlink_metadata(destination)
          .await
          .map_err(|error| io_error("inspect concurrently published action", destination, error))?;
        if !metadata.file_type().is_file()
          || metadata.file_type().is_symlink()
          || metadata.len() > MAX_ACTION_RESULT_WIRE_BYTES as u64
        {
          return Err(CacheError::Corrupt {
            path: destination.to_path_buf(),
            reason: "concurrently published action is not bounded regular metadata".to_owned(),
          });
        }
        let existing = tokio::fs::read(destination)
          .await
          .map_err(|error| io_error("read concurrently published action", destination, error))?;
        if existing == bytes {
          Ok(WriteOutcome::AlreadyPresent)
        } else {
          Ok(WriteOutcome::Conflict)
        }
      },
      WriteOutcome::Conflict => unreachable!("raw object publication does not compare values"),
    }
  }
}

fn create_cache_root(path: &Path) -> CacheResult<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => return Ok(()),
    Ok(_) => {
      return Err(CacheError::Configuration(format!(
        "local cache root '{}' must not be a file, symbolic link, junction, or reparse point",
        path.display()
      )))
    },
    Err(error) if error.kind() == io::ErrorKind::NotFound => {},
    Err(error) => return Err(io_error("inspect local cache root", path, error)),
  }
  // Parent directories belong to the operator-selected absolute path. The
  // root itself is checked again immediately after recursive creation.
  fs::create_dir_all(path).map_err(|error| io_error("create local cache root", path, error))?;
  let metadata =
    fs::symlink_metadata(path).map_err(|error| io_error("inspect created local cache root", path, error))?;
  if !metadata.is_dir() || is_link_or_reparse(&metadata) {
    return Err(CacheError::Configuration(format!(
      "local cache root '{}' changed while it was being created",
      path.display()
    )));
  }
  Ok(())
}

fn create_cache_directory(path: &Path) -> CacheResult<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => return Ok(()),
    Ok(_) => {
      return Err(CacheError::Configuration(format!(
        "local cache directory '{}' must not be a file, symbolic link, junction, or reparse point",
        path.display()
      )))
    },
    Err(error) if error.kind() == io::ErrorKind::NotFound => {},
    Err(error) => return Err(io_error("inspect local cache directory", path, error)),
  }
  // Another writer may create the same digest shard after our initial
  // inspection. Treat that race as success only after the common validation
  // below proves the winner created a real directory rather than a link.
  match fs::create_dir(path) {
    Ok(()) => {},
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
    Err(error) => return Err(io_error("create local cache directory", path, error)),
  }
  let metadata =
    fs::symlink_metadata(path).map_err(|error| io_error("inspect created local cache directory", path, error))?;
  if !metadata.is_dir() || is_link_or_reparse(&metadata) {
    return Err(CacheError::Configuration(format!(
      "local cache directory '{}' changed while it was being created",
      path.display()
    )));
  }
  Ok(())
}

/// Validates every dynamic directory below the fixed cache layout.
///
/// Namespace and digest shards are derived internally but pre-existing cache
/// contents are untrusted. Walking one component at a time prevents a stale
/// symlink or Windows reparse point from redirecting reads and publications
/// outside the operator-owned root. A missing ancestor is either created for a
/// writer or reported to a lookup without mutating the store.
fn validate_cache_parent(layout: &Path, object: &Path, create: bool) -> CacheResult<bool> {
  let parent = object
    .parent()
    .ok_or_else(|| CacheError::Configuration("cache object has no parent directory".to_owned()))?;
  let relative = parent.strip_prefix(layout).map_err(|_| {
    CacheError::Configuration(format!(
      "cache object '{}' is outside layout '{}'",
      object.display(),
      layout.display()
    ))
  })?;
  let mut current = layout.to_path_buf();
  for component in relative.components() {
    let std::path::Component::Normal(component) = component else {
      return Err(CacheError::Configuration(
        "cache object path contains a non-normal component".to_owned(),
      ));
    };
    current.push(component);
    match fs::symlink_metadata(&current) {
      Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => {},
      Ok(_) => {
        return Err(CacheError::Corrupt {
          path: current,
          reason: "cache object ancestor is not a regular directory".to_owned(),
        })
      },
      Err(error) if error.kind() == io::ErrorKind::NotFound && create => create_cache_directory(&current)?,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
      Err(error) => return Err(io_error("inspect cache object ancestor", &current, error)),
    }
  }
  Ok(true)
}

#[async_trait]
impl CacheStore for LocalCacheStore {
  async fn get_action(&self, namespace: &str, action: &Digest) -> CacheResult<Option<ActionLookup>> {
    let path = self.action_path(namespace, action)?;
    // Keep the shared GC lock through the sampled access update. Otherwise a
    // collector can remove the action after the read and leave an orphan
    // marker that makes a later action at the same path look artificially old.
    let gc_lock = acquire_lock(self.gc_lock_path(), false).await?;
    if !validate_cache_parent(&self.layout, &path, false)? {
      return Ok(None);
    }
    let metadata = match tokio::fs::symlink_metadata(&path).await {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(io_error("inspect action result", &path, error)),
    };
    if !metadata.file_type().is_file()
      || metadata.file_type().is_symlink()
      || metadata.len() > MAX_ACTION_RESULT_WIRE_BYTES as u64
    {
      drop(gc_lock);
      let reason = format!("action metadata exceeds {MAX_ACTION_RESULT_WIRE_BYTES} bytes or is not a regular file");
      return Err(self.quarantine_action(path, reason).await);
    }
    let bytes = tokio::fs::read(&path)
      .await
      .map_err(|error| io_error("read action result", &path, error))?;
    let result = match serde_json::from_slice::<ActionResultV1>(&bytes) {
      Ok(result) => result,
      Err(error) => {
        drop(gc_lock);
        let reason = format!("action metadata cannot be decoded: {error}");
        return Err(self.quarantine_action(path, reason).await);
      },
    };
    if let Err(error) = result.validate() {
      drop(gc_lock);
      return Err(self.quarantine_action(path, error.to_string()).await);
    }
    if &result.action != action {
      drop(gc_lock);
      let reason = "stored action result is bound to a different digest".to_owned();
      return Err(self.quarantine_action(path, reason).await);
    }
    record_sampled_access(&path, self.config.access_update_interval).await?;
    drop(gc_lock);
    Ok(Some(ActionLookup {
      result,
      layer: octa_cache_protocol::CacheLayer::Local,
    }))
  }

  async fn find_missing_blobs(&self, blobs: &[BlobDescriptor]) -> CacheResult<Vec<BlobDescriptor>> {
    let mut missing = Vec::new();
    let mut corrupt = Vec::new();
    {
      let _gc = acquire_lock(self.gc_lock_path(), false).await?;
      for blob in blobs {
        blob.validate()?;
        let path = self.blob_path(blob);
        if !validate_cache_parent(&self.layout, &path, false)? {
          missing.push(blob.clone());
          continue;
        }
        match tokio::fs::symlink_metadata(&path).await {
          Ok(metadata)
            if metadata.file_type().is_file()
              && !metadata.file_type().is_symlink()
              && metadata.len() == blob.encoded_size_bytes => {},
          Ok(_) => {
            missing.push(blob.clone());
            corrupt.push((
              blob.clone(),
              "encoded blob size or file kind differs from its descriptor",
            ));
          },
          Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(blob.clone()),
          Err(error) => return Err(io_error("inspect local cache blob", path, error)),
        }
      }
    }
    for (blob, reason) in corrupt {
      self.quarantine_blob(&blob, reason).await?;
    }
    Ok(missing)
  }

  async fn read_blob(&self, blob: &BlobDescriptor) -> CacheResult<BlobReader> {
    blob.validate()?;
    let path = self.blob_path(blob);
    let gc_lock = acquire_lock(self.gc_lock_path(), false).await?;
    if !validate_cache_parent(&self.layout, &path, false)? {
      return Err(io_error(
        "open local cache blob",
        &path,
        io::Error::new(io::ErrorKind::NotFound, "cache blob directory does not exist"),
      ));
    }
    let path_metadata = tokio::fs::symlink_metadata(&path)
      .await
      .map_err(|error| io_error("inspect local cache blob", &path, error))?;
    if !path_metadata.file_type().is_file()
      || path_metadata.file_type().is_symlink()
      || path_metadata.len() != blob.encoded_size_bytes
    {
      drop(gc_lock);
      self
        .quarantine_blob(blob, "encoded blob size or file kind differs from its descriptor")
        .await?;
      return Err(CacheError::Corrupt {
        path,
        reason: "encoded blob size or file kind differs from its descriptor".to_owned(),
      });
    }
    let file = tokio::fs::File::open(&path)
      .await
      .map_err(|error| io_error("open local cache blob", &path, error))?;
    let metadata = file
      .metadata()
      .await
      .map_err(|error| io_error("inspect local cache blob", &path, error))?;
    if !metadata.is_file() || metadata.len() != blob.encoded_size_bytes {
      drop(file);
      drop(gc_lock);
      self
        .quarantine_blob(blob, "encoded blob size or file kind differs from its descriptor")
        .await?;
      return Err(CacheError::Corrupt {
        path,
        reason: "encoded blob size or file kind differs from its descriptor".to_owned(),
      });
    }
    Ok(Box::pin(LockedBlobReader::new(file, gc_lock)))
  }

  async fn write_blob_if_absent(&self, blob: &BlobDescriptor, body: BlobReader) -> CacheResult<WriteOutcome> {
    blob.validate()?;
    self.validate_blob_resources(blob)?;
    if self.existing_blob_is_valid(blob).await? {
      return Ok(WriteOutcome::AlreadyPresent);
    }
    let gc_lock = self.reserve_capacity(blob.encoded_size_bytes).await?;
    let result = self.write_blob_reserved(blob, body, gc_lock).await;
    self.finish_reservation(blob.encoded_size_bytes, result).await
  }

  async fn write_action_if_absent(&self, namespace: &str, result: &ActionResultV1) -> CacheResult<WriteOutcome> {
    result.validate()?;
    let destination = self.action_path(namespace, &result.action)?;
    // `ActionResultV1` contains only plain serde DTOs and `serde_json::Value`,
    // none of which has a fallible custom serializer or an I/O destination.
    let bytes = serde_json::to_vec(result).expect("validated action-result metadata must serialize to JSON");
    let size = bytes.len() as u64;
    let gc_lock = self.reserve_capacity(size).await?;
    let publication = self.write_action_reserved(&destination, result, &bytes, gc_lock).await;
    self.finish_reservation(size, publication).await
  }
}

async fn verify_blob_file(path: PathBuf, descriptor: BlobDescriptor, max_expanded_bytes: u64) -> CacheResult<()> {
  tokio::task::spawn_blocking(move || verify_encoded_blob(&path, &descriptor, max_expanded_bytes))
    .await
    .map_err(CacheError::Worker)?
}

fn validate_action_digest(action: &Digest) -> CacheResult<()> {
  if action.algorithm() != DigestAlgorithm::Blake3 {
    return Err(CacheError::Configuration(
      "local action keys must use BLAKE3".to_owned(),
    ));
  }
  Ok(())
}

fn namespace_directory(namespace: &str) -> CacheResult<String> {
  validate_namespace(namespace)?;
  Ok(blake3::hash(namespace.as_bytes()).to_hex().to_string())
}

#[cfg(test)]
mod tests;
