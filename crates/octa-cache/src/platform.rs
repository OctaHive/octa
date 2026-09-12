//! Portable filesystem state and small platform-specific operations.

use std::{
  fs::{File, Metadata},
  path::Path,
  time::{SystemTime, UNIX_EPOCH},
};

use crate::{error::io_error, CacheResult};

/// Returns whether metadata represents a path indirection that must not occur
/// in the operator-owned cache layout.
///
/// Windows directory junctions are reparse points but are not always reported
/// as ordinary symbolic links by the standard-library file type.
pub(crate) fn is_link_or_reparse(metadata: &Metadata) -> bool {
  if metadata.file_type().is_symlink() {
    return true;
  }
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
  }
  #[cfg(not(windows))]
  false
}

/// Confirms that an opened handle still denotes the current path entry.
pub(crate) fn is_current_file(path: &Path, file: &File) -> CacheResult<bool> {
  let opened = same_file::Handle::from_file(
    file
      .try_clone()
      .map_err(|error| io_error("retain opened cache file", path, error))?,
  )
  .map_err(|error| io_error("identify opened cache file", path, error))?;
  let current = same_file::Handle::from_path(path).map_err(|error| io_error("identify cache path", path, error))?;
  Ok(opened == current)
}

/// Makes a preceding rename or file creation durable on platforms that expose
/// directory synchronization through a regular file handle.
pub(crate) fn sync_directory(path: &Path) -> CacheResult<()> {
  #[cfg(unix)]
  {
    std::fs::File::open(path)
      .and_then(|directory| directory.sync_all())
      .map_err(|error| io_error("synchronize cache directory", path, error))?;
  }
  #[cfg(not(unix))]
  let _ = path;
  Ok(())
}

/// Process-local identity used to coalesce reads and detect entry replacement.
/// It is never persisted and never replaces content hashing.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum FileIdentity {
  #[cfg(unix)]
  Unix { device: u64, inode: u64 },
  #[cfg(windows)]
  Windows { volume: u64, index: u64 },
  #[cfg(not(unix))]
  Path(std::path::PathBuf),
}

/// Process-local metadata fingerprint used for optimistic mutation detection.
///
/// It deliberately is not persisted and never replaces content hashing. Both
/// input hashing and output capture use it to detect replacement or mutation
/// of a filesystem entry while its bytes are being consumed.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct EntryKey {
  identity: FileIdentity,
  length: u64,
  modified: (bool, u64, u32),
  change: PlatformChange,
}

impl EntryKey {
  pub(crate) fn new(path: &Path, metadata: &Metadata) -> CacheResult<Self> {
    Ok(Self {
      identity: file_identity(path, metadata),
      length: metadata.len(),
      modified: system_time_key(
        metadata
          .modified()
          .map_err(|error| io_error("read filesystem entry modification time", path, error))?,
      ),
      change: platform_change(metadata),
    })
  }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PlatformChange(i64, i64);

#[cfg(unix)]
fn platform_change(metadata: &Metadata) -> PlatformChange {
  use std::os::unix::fs::MetadataExt as _;
  PlatformChange(metadata.ctime(), metadata.ctime_nsec())
}

#[cfg(not(unix))]
fn platform_change(metadata: &Metadata) -> PlatformChange {
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt as _;
    // Creation time distinguishes a replacement at the same path when a
    // filesystem cannot provide a stable file index. Attributes complement
    // the portable executable bit and catch relevant metadata changes.
    return PlatformChange(metadata.creation_time() as i64, metadata.file_attributes() as i64);
  }
  #[cfg(not(windows))]
  {
    let _ = metadata;
    PlatformChange(0, 0)
  }
}

fn system_time_key(time: SystemTime) -> (bool, u64, u32) {
  match time.duration_since(UNIX_EPOCH) {
    Ok(duration) => (false, duration.as_secs(), duration.subsec_nanos()),
    Err(error) => {
      let duration = error.duration();
      (true, duration.as_secs(), duration.subsec_nanos())
    },
  }
}

#[cfg(unix)]
pub(crate) fn file_identity(_path: &Path, metadata: &Metadata) -> FileIdentity {
  use std::os::unix::fs::MetadataExt as _;
  FileIdentity::Unix {
    device: metadata.dev(),
    inode: metadata.ino(),
  }
}

#[cfg(windows)]
pub(crate) fn file_identity(path: &Path, _metadata: &Metadata) -> FileIdentity {
  // The equivalent methods on `std::os::windows::fs::MetadataExt` are still
  // unstable. Query the same stable Win32 handle information through
  // `winapi-util`, which also knows how to open directories correctly.
  let identity = winapi_util::Handle::from_path_any(path)
    .and_then(|handle| winapi_util::file::information(&handle))
    .map(|information| (information.volume_serial_number(), information.file_index()));
  windows_file_identity(path, identity.ok())
}

#[cfg(windows)]
fn windows_file_identity(path: &Path, identity: Option<(u64, u64)>) -> FileIdentity {
  match identity {
    Some((volume, index)) => FileIdentity::Windows { volume, index },
    // Never coalesce unrelated files when handle information is unavailable.
    // The path fallback may miss hard-link reuse, but it cannot reuse another
    // directory entry's content digest.
    _ => FileIdentity::Path(path.to_path_buf()),
  }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn file_identity(path: &Path, _metadata: &Metadata) -> FileIdentity {
  FileIdentity::Path(path.to_path_buf())
}

impl EntryKey {
  /// Adds this process-local fingerprint to a transient tree snapshot.
  ///
  /// This encoding is never persisted. It only lets the bundle writer compare
  /// two metadata walks performed by the same process and platform.
  pub(crate) fn update_fingerprint(&self, hasher: &mut blake3::Hasher) {
    match &self.identity {
      #[cfg(unix)]
      FileIdentity::Unix { device, inode } => {
        hasher.update(&[1]);
        hasher.update(&device.to_be_bytes());
        hasher.update(&inode.to_be_bytes());
      },
      #[cfg(windows)]
      FileIdentity::Windows { volume, index } => {
        hasher.update(&[2]);
        hasher.update(&volume.to_be_bytes());
        hasher.update(&index.to_be_bytes());
      },
      #[cfg(not(unix))]
      FileIdentity::Path(path) => {
        hasher.update(&[3]);
        hasher.update(path.to_string_lossy().as_bytes());
      },
    }
    hasher.update(&self.length.to_be_bytes());
    hasher.update(&[u8::from(self.modified.0)]);
    hasher.update(&self.modified.1.to_be_bytes());
    hasher.update(&self.modified.2.to_be_bytes());
    hasher.update(&self.change.0.to_be_bytes());
    hasher.update(&self.change.1.to_be_bytes());
  }
}

#[cfg(unix)]
/// Captures the portable executable semantic from any Unix execute bit.
pub(crate) fn executable(metadata: &Metadata) -> bool {
  use std::os::unix::fs::PermissionsExt as _;
  metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
/// Windows cache identity does not model an executable permission bit.
pub(crate) fn executable(_metadata: &Metadata) -> bool {
  false
}

#[cfg(unix)]
/// Restores normalized executable or non-executable file permissions.
///
/// Bundles intentionally preserve only executable semantics, not host-specific
/// ownership, ACLs, or the producer's complete mode.
pub(crate) fn set_executable(path: &Path, executable: bool) -> std::io::Result<()> {
  use std::os::unix::fs::PermissionsExt as _;
  let mode = if executable { 0o755 } else { 0o644 };
  std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
/// Executability has no portable permission representation on Windows.
pub(crate) fn set_executable(_path: &Path, _executable: bool) -> std::io::Result<()> {
  Ok(())
}

#[cfg(unix)]
/// Creates a relative symlink; Unix does not encode the target entry kind.
pub(crate) fn create_symlink(target: &Path, destination: &Path, _directory: bool) -> std::io::Result<()> {
  std::os::unix::fs::symlink(target, destination)
}

#[cfg(windows)]
/// Creates the Windows link kind recorded while the bundle was captured.
pub(crate) fn create_symlink(target: &Path, destination: &Path, directory: bool) -> std::io::Result<()> {
  use std::os::windows::fs::{symlink_dir, symlink_file};
  if directory {
    symlink_dir(target, destination)
  } else {
    symlink_file(target, destination)
  }
}

#[cfg(test)]
mod tests {
  #[cfg(windows)]
  use std::path::Path;
  use std::time::Duration;

  use super::*;

  #[test]
  fn timestamps_before_the_epoch_have_a_distinct_stable_key() {
    assert_eq!(system_time_key(UNIX_EPOCH - Duration::from_nanos(1)), (true, 0, 1));
    assert_eq!(system_time_key(UNIX_EPOCH + Duration::from_nanos(1)), (false, 0, 1));
  }

  #[cfg(windows)]
  #[test]
  fn unavailable_windows_file_ids_fall_back_to_the_exact_path() {
    let first = windows_file_identity(Path::new(r"C:\workspace\first"), None);
    let second = windows_file_identity(Path::new(r"C:\workspace\second"), None);
    assert_ne!(first, second);
    assert_eq!(
      windows_file_identity(Path::new(r"C:\workspace\first"), Some((7, 11))),
      windows_file_identity(Path::new(r"D:\other"), Some((7, 11)))
    );
  }

  #[cfg(windows)]
  #[test]
  fn reads_windows_file_identity_through_a_stable_handle_api() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entry");
    std::fs::write(&path, b"cache input").unwrap();

    assert!(matches!(
      file_identity(&path, &std::fs::metadata(&path).unwrap()),
      FileIdentity::Windows { .. }
    ));
  }
}
