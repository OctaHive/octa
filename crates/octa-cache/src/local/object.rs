//! Low-level object verification, locking, and durability helpers.

use std::{
  fs::{self, File, OpenOptions},
  io::{self, Read},
  path::{Path, PathBuf},
  pin::Pin,
  task::{Context, Poll},
  time::Duration,
};

use octa_cache_protocol::{BlobDescriptor, BlobEncoding, Digest, DigestAlgorithm, ZSTD_V1_MAX_WINDOW_LOG};
use tokio::io::{AsyncRead, ReadBuf};

use super::BLOB_EXTENSION;
use crate::{
  error::io_error,
  platform::{is_current_file, is_link_or_reparse},
  CacheError, CacheResult,
};

const VERIFICATION_BUFFER_BYTES: usize = 1024 * 1024;

/// Blob reader that keeps its shared GC lease until the stream is dropped.
///
/// Opening the file alone is insufficient on every supported platform: GC must
/// not unlink or quarantine its path between the store's validation and the
/// consumer finishing the encoded stream.
pub(super) struct LockedBlobReader {
  file: tokio::fs::File,
  _gc_lock: File,
}

impl LockedBlobReader {
  pub(super) fn new(file: tokio::fs::File, gc_lock: File) -> Self {
    Self {
      file,
      _gc_lock: gc_lock,
    }
  }
}

impl AsyncRead for LockedBlobReader {
  fn poll_read(mut self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    Pin::new(&mut self.file).poll_read(context, buffer)
  }
}

/// Includes physical representation metadata because two valid encodings of
/// one semantic digest can have different byte lengths.
pub(crate) fn blob_relative_path(blob: &BlobDescriptor) -> PathBuf {
  let encoding = match blob.encoding {
    BlobEncoding::Identity => "identity",
    BlobEncoding::ZstdV1 => "zstd-v1",
  };
  let hash = blob.digest.hex();
  PathBuf::from("blobs").join("blake3").join(&hash[..2]).join(format!(
    "{}-{}-{encoding}-{}.{}",
    hash, blob.expanded_size_bytes, blob.encoded_size_bytes, BLOB_EXTENSION
  ))
}

/// Verifies producer bytes before they can become a durable CAS object.
/// Bundle extraction later validates canonical entry structure and paths; this
/// pass establishes the descriptor's physical size and semantic byte identity.
pub(super) fn verify_encoded_blob(
  path: &Path,
  descriptor: &BlobDescriptor,
  max_expanded_bytes: u64,
) -> CacheResult<()> {
  if descriptor.expanded_size_bytes > max_expanded_bytes {
    return Err(CacheError::Limit(format!(
      "blob expands to {} bytes, above the configured limit of {max_expanded_bytes}",
      descriptor.expanded_size_bytes
    )));
  }
  let file = File::open(path).map_err(|error| io_error("open temporary blob for verification", path, error))?;
  let encoded_size = file
    .metadata()
    .map_err(|error| io_error("inspect temporary blob for verification", path, error))?
    .len();
  if encoded_size != descriptor.encoded_size_bytes {
    return Err(CacheError::InvalidBundle(format!(
      "encoded blob has {encoded_size} bytes, expected {}",
      descriptor.encoded_size_bytes
    )));
  }
  let mut reader: Box<dyn Read> = match descriptor.encoding {
    BlobEncoding::Identity => Box::new(file),
    BlobEncoding::ZstdV1 => {
      let mut decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| CacheError::InvalidBundle(format!("cannot decode Zstandard blob: {error}")))?;
      decoder
        .window_log_max(ZSTD_V1_MAX_WINDOW_LOG)
        .map_err(|error| CacheError::InvalidBundle(format!("cannot bound Zstandard decoder window: {error}")))?;
      Box::new(decoder)
    },
  };
  let mut hasher = blake3::Hasher::new();
  let mut expanded = 0_u64;
  let mut buffer = vec![0_u8; VERIFICATION_BUFFER_BYTES];
  loop {
    let read = reader
      .read(&mut buffer)
      .map_err(|error| CacheError::InvalidBundle(format!("cannot verify encoded blob: {error}")))?;
    if read == 0 {
      break;
    }
    expanded = expanded
      .checked_add(read as u64)
      .ok_or_else(|| CacheError::Limit("expanded blob size overflowed".to_owned()))?;
    if expanded > descriptor.expanded_size_bytes {
      return Err(CacheError::InvalidBundle(
        "blob expands beyond its declared size".to_owned(),
      ));
    }
    hasher.update(&buffer[..read]);
  }
  let actual = Digest::new(DigestAlgorithm::Blake3, *hasher.finalize().as_bytes(), expanded);
  if actual != descriptor.digest {
    return Err(CacheError::InvalidBundle(
      "expanded blob digest or size differs from its descriptor".to_owned(),
    ));
  }
  Ok(())
}

/// Acquires an advisory cross-process lock without blocking a Tokio worker.
pub(crate) async fn acquire_lock(path: PathBuf, exclusive: bool) -> CacheResult<File> {
  tokio::task::spawn_blocking(move || {
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent).map_err(|error| io_error("create cache lock directory", parent, error))?;
    }
    let file = open_lock_file(&path)?;
    if exclusive {
      fs2::FileExt::lock_exclusive(&file).map_err(|error| io_error("lock cache object", &path, error))?;
    } else {
      fs2::FileExt::lock_shared(&file).map_err(|error| io_error("lock cache for reading", &path, error))?;
    }
    Ok(file)
  })
  .await
  .map_err(CacheError::Worker)?
}

/// Opens a lock leaf without following an existing symlink or reparse point.
///
/// The initial `create_new` is atomic. Reused leaves are inspected before and
/// after opening so untrusted cache contents cannot redirect a lock outside the
/// operator-owned layout.
pub(super) fn open_lock_file(path: &Path) -> CacheResult<File> {
  let options = || {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    options
  };
  match options().create_new(true).open(path) {
    Ok(file) => return Ok(file),
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
    Err(error) => return Err(io_error("create cache lock", path, error)),
  }
  let path_metadata = fs::symlink_metadata(path).map_err(|error| io_error("inspect cache lock", path, error))?;
  if !path_metadata.is_file() || is_link_or_reparse(&path_metadata) {
    return Err(CacheError::Configuration(format!(
      "cache lock '{}' must be a regular non-link file",
      path.display()
    )));
  }
  let file = options()
    .open(path)
    .map_err(|error| io_error("open cache lock", path, error))?;
  let opened_metadata = file
    .metadata()
    .map_err(|error| io_error("inspect opened cache lock", path, error))?;
  let current_metadata = fs::symlink_metadata(path).map_err(|error| io_error("re-inspect cache lock", path, error))?;
  let same = is_current_file(path, &file)?;
  if !same || !opened_metadata.is_file() || !current_metadata.is_file() || is_link_or_reparse(&current_metadata) {
    return Err(CacheError::Configuration(format!(
      "cache lock '{}' changed while it was opened",
      path.display()
    )));
  }
  Ok(file)
}

/// Updates recency at most once per configured interval instead of writing on
/// every cache hit.
pub(super) async fn record_sampled_access(path: &Path, interval: Duration) -> CacheResult<()> {
  let marker = path.with_extension("access");
  let parent = marker
    .parent()
    .ok_or_else(|| CacheError::Configuration("cache action access marker has no parent directory".to_owned()))?;
  match tokio::fs::symlink_metadata(parent).await {
    Ok(metadata) if metadata.file_type().is_dir() && !is_link_or_reparse(&metadata) => {},
    Ok(_) => {
      return Err(io_error(
        "inspect cache action access marker parent",
        parent,
        io::Error::new(io::ErrorKind::NotADirectory, "parent is not a regular directory"),
      ));
    },
    // An operator may remove the cache while a lookup is finishing. Access
    // sampling is advisory, so an absent shard must not turn a hit into an
    // execution failure.
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
    Err(error) => return Err(io_error("inspect cache action access marker parent", parent, error)),
  }
  let recent = match tokio::fs::symlink_metadata(&marker).await {
    Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => metadata
      .modified()
      .ok()
      .and_then(|modified| modified.elapsed().ok())
      .is_some_and(|age| age < interval),
    Ok(_) => false,
    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
    Err(error) => return Err(io_error("inspect cache action access marker", &marker, error)),
  };
  if recent {
    return Ok(());
  }
  match tokio::fs::remove_file(&marker).await {
    Ok(()) => {},
    Err(error) if error.kind() == io::ErrorKind::NotFound => {},
    Err(error) => return Err(io_error("remove stale cache action access marker", &marker, error)),
  }
  match tokio::fs::OpenOptions::new()
    .create_new(true)
    .write(true)
    .open(&marker)
    .await
  {
    Ok(_) => Ok(()),
    // Another reader may have refreshed the same sampled marker after our
    // removal. Its marker is equivalent to ours.
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
    // GC can remove the action shard after a caller releases its shared lock;
    // access sampling must never turn that benign race into a cache failure.
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(io_error("update cache action access marker", marker, error)),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn descriptor(bytes: &[u8]) -> BlobDescriptor {
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

  #[test]
  fn encoded_blob_verification_rejects_limits_and_a_physical_size_mismatch() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("blob");
    fs::write(&path, b"short").unwrap();
    let expected = descriptor(b"longer bytes");
    assert!(matches!(
      verify_encoded_blob(&path, &expected, expected.expanded_size_bytes - 1),
      Err(CacheError::Limit(message)) if message.contains("above the configured limit")
    ));
    assert!(matches!(
      verify_encoded_blob(&path, &expected, 1024),
      Err(CacheError::InvalidBundle(message)) if message.contains("encoded blob has")
    ));
  }

  #[tokio::test]
  async fn locks_create_their_parent_and_support_both_lock_modes() {
    let root = tempfile::tempdir().unwrap();
    let exclusive_path = root.path().join("nested/exclusive.lock");
    let exclusive = acquire_lock(exclusive_path.clone(), true).await.unwrap();
    assert!(exclusive_path.is_file());
    drop(exclusive);

    let shared_path = root.path().join("nested/shared.lock");
    let first = acquire_lock(shared_path.clone(), false).await.unwrap();
    let second = acquire_lock(shared_path.clone(), false).await.unwrap();
    assert!(shared_path.is_file());
    drop((first, second));

    assert!(matches!(
      open_lock_file(&root.path().join("missing/lock")),
      Err(CacheError::Io { .. })
    ));
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn lock_opening_rejects_a_symbolic_link_leaf() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    let path = root.path().join("lock");
    symlink(outside.path(), &path).unwrap();

    assert!(matches!(
      acquire_lock(path, true).await,
      Err(CacheError::Configuration(_))
    ));
  }

  #[tokio::test]
  async fn access_sampling_is_idempotent_and_tolerates_a_removed_parent() {
    let root = tempfile::tempdir().unwrap();
    let action = root.path().join("action.json");
    fs::write(&action, b"action").unwrap();
    record_sampled_access(&action, Duration::from_secs(60)).await.unwrap();
    let marker = action.with_extension("access");
    let first_modified = fs::metadata(&marker).unwrap().modified().unwrap();
    record_sampled_access(&action, Duration::from_secs(60)).await.unwrap();
    assert_eq!(fs::metadata(&marker).unwrap().modified().unwrap(), first_modified);

    let removed = root.path().join("removed/action.json");
    record_sampled_access(&removed, Duration::from_secs(60)).await.unwrap();
    assert!(!removed.with_extension("access").exists());

    let non_directory = root.path().join("file");
    fs::write(&non_directory, b"not a directory").unwrap();
    assert!(matches!(
      record_sampled_access(&non_directory.join("action.json"), Duration::from_secs(60)).await,
      Err(CacheError::Io { .. })
    ));

    let directory_marker_action = root.path().join("directory-marker.json");
    fs::create_dir(directory_marker_action.with_extension("access")).unwrap();
    assert!(matches!(
      record_sampled_access(&directory_marker_action, Duration::from_secs(60)).await,
      Err(CacheError::Io { .. })
    ));
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn access_sampling_replaces_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let action = root.path().join("action.json");
    let marker = action.with_extension("access");
    let target = outside.path().join("target");
    fs::write(&action, b"action").unwrap();
    fs::write(&target, b"outside").unwrap();
    symlink(&target, &marker).unwrap();

    record_sampled_access(&action, Duration::from_secs(60)).await.unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"outside");
    assert!(fs::symlink_metadata(&marker).unwrap().file_type().is_file());
    assert!(!fs::symlink_metadata(&marker).unwrap().file_type().is_symlink());
  }
}
