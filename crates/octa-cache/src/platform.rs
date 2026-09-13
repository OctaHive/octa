//! Portable filesystem state and small platform-specific operations.

use std::{
  fs::{File, Metadata},
  path::Path,
  time::{SystemTime, UNIX_EPOCH},
};

use crate::{error::io_error, CacheResult};

#[cfg(any(target_os = "linux", target_os = "macos"))]
const MEMO_SCOPE_DOMAIN: &[u8] = b"octa.input-memo-scope.v1";

/// Frames the host-local values that make persisted metadata reusable.
///
/// The scope is not a portable cache identity. Length-prefixing still matters:
/// it prevents two different filesystem/type/boot tuples from producing the
/// same byte stream and lets this private format evolve by changing its domain.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn memo_scope(filesystem_type: &[u8], filesystem_id: &[u8], boot: &[u8]) -> Vec<u8> {
  let mut scope = Vec::with_capacity(
    MEMO_SCOPE_DOMAIN.len() + filesystem_type.len() + filesystem_id.len() + boot.len() + 4 * std::mem::size_of::<u32>(),
  );
  for value in [MEMO_SCOPE_DOMAIN, filesystem_type, filesystem_id, boot] {
    scope.extend_from_slice(&(value.len() as u32).to_be_bytes());
    scope.extend_from_slice(value);
  }
  scope
}

/// Views the opaque libc filesystem identifier as host-local bytes.
///
/// Linux and Darwin define `fsid_t` as two adjacent 32-bit integers, but libc
/// intentionally hides their fields on some targets. Its complete ABI value is
/// stable for the lifetime of the mounted filesystem, which is exactly the
/// scope required here; these bytes never enter a portable action key or cross
/// machines.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn filesystem_id_bytes(value: &libc::fsid_t) -> &[u8] {
  // SAFETY: the platform ABI defines fsid_t as two adjacent i32 values, so its
  // representation has no padding. The returned slice cannot outlive `value`.
  unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast(), std::mem::size_of::<libc::fsid_t>()) }
}

/// Returns a process-restart-safe scope for optimistic digest reuse.
///
/// The scope exists only on vetted local filesystems and is bound to the
/// current operating-system boot. Rebooting, mounting a different filesystem,
/// or using a remote/unknown filesystem therefore disables existing memo
/// records and falls back to content hashing.
#[cfg(target_os = "linux")]
pub(crate) fn persistent_memo_scope(path: &Path) -> Option<Vec<u8>> {
  use std::{os::fd::AsRawFd as _, sync::OnceLock};

  // Linux filesystem magic values are a stable userspace ABI. Keep this list
  // deliberately conservative: an unknown local filesystem loses an
  // optimization, while accidentally accepting a remote one could reuse stale
  // attribute-cache data.
  // These values are the Linux `statfs(2)` userspace ABI. Define them locally
  // because libc exposes an inconsistent subset across GNU, musl, and Android.
  const EXT_SUPER_MAGIC: libc::c_long = 0x0000_EF53;
  const XFS_SUPER_MAGIC: libc::c_long = 0x5846_5342;
  const BTRFS_SUPER_MAGIC: libc::c_long = 0x9123_683E_u32 as libc::c_long;
  const TMPFS_MAGIC: libc::c_long = 0x0102_1994;
  const OVERLAYFS_SUPER_MAGIC: libc::c_long = 0x794C_7630;
  const ZFS_SUPER_MAGIC: libc::c_long = 0x2FC1_2FC1;
  const F2FS_SUPER_MAGIC: libc::c_long = 0xF2F5_2010_u32 as libc::c_long;
  const LOCAL_FILESYSTEMS: &[libc::c_long] = &[
    EXT_SUPER_MAGIC,
    XFS_SUPER_MAGIC,
    BTRFS_SUPER_MAGIC,
    TMPFS_MAGIC,
    OVERLAYFS_SUPER_MAGIC,
    ZFS_SUPER_MAGIC,
    F2FS_SUPER_MAGIC,
  ];
  static BOOT_ID: OnceLock<Option<Vec<u8>>> = OnceLock::new();

  let directory = File::open(path).ok()?;
  let mut statistics = std::mem::MaybeUninit::<libc::statfs>::zeroed();
  // SAFETY: `statistics` points to writable storage of the exact type expected
  // by fstatfs, and `directory` keeps the descriptor valid for this call.
  if unsafe { libc::fstatfs(directory.as_raw_fd(), statistics.as_mut_ptr()) } != 0 {
    return None;
  }
  // SAFETY: a successful fstatfs initialized the complete structure. The
  // buffer started zeroed, so any ABI padding also has defined byte contents.
  let statistics = unsafe { statistics.assume_init() };
  let filesystem = statistics.f_type;
  if !LOCAL_FILESYSTEMS.contains(&filesystem) {
    return None;
  }
  let boot = BOOT_ID
    .get_or_init(|| {
      let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
      let value = value.trim();
      (value.len() == 36 && value.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-'))
        .then(|| value.as_bytes().to_vec())
    })
    .as_ref()?;
  Some(memo_scope(
    &filesystem.to_be_bytes(),
    filesystem_id_bytes(&statistics.f_fsid),
    boot,
  ))
}

#[cfg(target_os = "macos")]
pub(crate) fn persistent_memo_scope(path: &Path) -> Option<Vec<u8>> {
  use std::{ffi::CStr, os::fd::AsRawFd as _, sync::OnceLock};

  static BOOT_TIME: OnceLock<Option<[u8; 16]>> = OnceLock::new();

  let directory = File::open(path).ok()?;
  let mut statistics = std::mem::MaybeUninit::<libc::statfs>::zeroed();
  // SAFETY: `statistics` is correctly sized writable storage and the directory
  // descriptor remains live for the duration of fstatfs.
  if unsafe { libc::fstatfs(directory.as_raw_fd(), statistics.as_mut_ptr()) } != 0 {
    return None;
  }
  // SAFETY: successful fstatfs initializes the structure and guarantees a
  // NUL-terminated filesystem name in f_fstypename.
  let statistics = unsafe { statistics.assume_init() };
  let filesystem = unsafe { CStr::from_ptr(statistics.f_fstypename.as_ptr()) }.to_bytes();
  if !matches!(filesystem, b"apfs" | b"hfs") {
    return None;
  }
  let boot = BOOT_TIME
    .get_or_init(|| {
      let mut value = std::mem::MaybeUninit::<libc::timeval>::zeroed();
      let mut length = std::mem::size_of::<libc::timeval>();
      // SAFETY: the name is NUL-terminated, `value` and `length` describe the
      // output buffer, and this read-only sysctl has no new-value pointer.
      let result = unsafe {
        libc::sysctlbyname(
          c"kern.boottime".as_ptr(),
          value.as_mut_ptr().cast(),
          &mut length,
          std::ptr::null_mut(),
          0,
        )
      };
      if result != 0 || length != std::mem::size_of::<libc::timeval>() {
        return None;
      }
      // SAFETY: successful sysctlbyname initialized the complete timeval.
      let value = unsafe { value.assume_init() };
      let mut encoded = [0_u8; 16];
      encoded[..8].copy_from_slice(&value.tv_sec.to_be_bytes());
      encoded[8..].copy_from_slice(&(value.tv_usec as i64).to_be_bytes());
      Some(encoded)
    })
    .as_ref()?;
  Some(memo_scope(filesystem, filesystem_id_bytes(&statistics.f_fsid), boot))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn persistent_memo_scope(_path: &Path) -> Option<Vec<u8>> {
  // Windows and other hosts keep full content hashing until Octa can bind
  // memo records to an equally reliable boot and local-volume identity.
  None
}

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
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
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

/// Filesystem identity used to detect entry replacement.
/// The raw value is never persisted and never replaces content hashing; only
/// a domain-separated fingerprint can enter the local digest memo.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum FileIdentity {
  #[cfg(unix)]
  Unix { device: u64, inode: u64 },
  #[cfg(windows)]
  Windows { volume: u64, index: u64 },
  #[cfg(not(unix))]
  Path(std::path::PathBuf),
}

/// Metadata fingerprint used for optimistic mutation detection.
///
/// The raw fields deliberately are not persisted and never replace content
/// hashing. Both input hashing and output capture use them to detect
/// replacement or mutation while bytes are consumed. `persistent` is true only
/// when the host supplied both a stable file identity and a change timestamp.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct EntryKey {
  identity: FileIdentity,
  length: u64,
  modified: (bool, u64, u32),
  change: PlatformChange,
  persistent: bool,
}

impl EntryKey {
  pub(crate) fn new(path: &Path, metadata: &Metadata) -> CacheResult<Self> {
    let (identity, change, persistent) = platform_identity(path, metadata);
    Ok(Self {
      identity,
      length: metadata.len(),
      modified: system_time_key(
        metadata
          .modified()
          .map_err(|error| io_error("read filesystem entry modification time", path, error))?,
      ),
      change,
      persistent,
    })
  }

  pub(crate) fn length(&self) -> u64 {
    self.length
  }

  /// Adds a stable local identity to a larger metadata-tree digest.
  ///
  /// The returned byte count is the exact number appended to `hasher`. `None`
  /// means this filesystem did not expose enough identity information for
  /// reuse across processes. Folding the fields directly into the tree hasher
  /// avoids constructing and finalizing a separate BLAKE3 state per file.
  pub(crate) fn update_memo_fingerprint(&self, hasher: &mut blake3::Hasher) -> Option<u64> {
    if !self.persistent {
      return None;
    }
    Some(self.update_fingerprint(hasher))
  }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PlatformChange(i64, i64);

#[cfg(unix)]
fn platform_identity(_path: &Path, metadata: &Metadata) -> (FileIdentity, PlatformChange, bool) {
  use std::os::unix::fs::MetadataExt as _;
  (
    FileIdentity::Unix {
      device: metadata.dev(),
      inode: metadata.ino(),
    },
    PlatformChange(metadata.ctime(), metadata.ctime_nsec()),
    true,
  )
}

#[cfg(windows)]
fn platform_identity(path: &Path, metadata: &Metadata) -> (FileIdentity, PlatformChange, bool) {
  use std::mem::size_of;
  use winapi_util::AsHandleRef as _;
  use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO},
  };

  let Ok(handle) = winapi_util::Handle::from_path_any(path) else {
    return windows_fallback_identity(path, metadata);
  };
  let Ok(information) = winapi_util::file::information(&handle) else {
    return windows_fallback_identity(path, metadata);
  };
  let mut basic = FILE_BASIC_INFO::default();
  // SAFETY: `basic` is a correctly sized writable FILE_BASIC_INFO and the
  // borrowed handle remains valid for the duration of this synchronous call.
  let succeeded = unsafe {
    GetFileInformationByHandleEx(
      handle.as_raw() as HANDLE,
      FileBasicInfo,
      std::ptr::addr_of_mut!(basic).cast(),
      size_of::<FILE_BASIC_INFO>() as u32,
    )
  };
  if succeeded == 0 {
    return windows_fallback_identity(path, metadata);
  }
  (
    FileIdentity::Windows {
      volume: information.volume_serial_number(),
      index: information.file_index(),
    },
    PlatformChange(basic.ChangeTime, basic.FileAttributes as i64),
    true,
  )
}

#[cfg(windows)]
fn windows_fallback_identity(path: &Path, metadata: &Metadata) -> (FileIdentity, PlatformChange, bool) {
  use std::os::windows::fs::MetadataExt as _;
  // A path fallback keeps ordinary hashing available on filesystems that do
  // not expose stable file IDs. Creation time and attributes still prevent
  // unrelated entries from sharing a transient identity in this process.
  (
    FileIdentity::Path(path.to_path_buf()),
    PlatformChange(metadata.creation_time() as i64, metadata.file_attributes() as i64),
    false,
  )
}

#[cfg(not(any(unix, windows)))]
fn platform_identity(path: &Path, _metadata: &Metadata) -> (FileIdentity, PlatformChange, bool) {
  (FileIdentity::Path(path.to_path_buf()), PlatformChange(0, 0), false)
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

impl EntryKey {
  /// Adds this process-local fingerprint to a transient tree snapshot.
  ///
  /// This encoding is never persisted. It only lets the bundle writer compare
  /// two metadata walks performed by the same process and platform.
  pub(crate) fn update_fingerprint(&self, hasher: &mut blake3::Hasher) -> u64 {
    let identity_bytes = match &self.identity {
      #[cfg(unix)]
      FileIdentity::Unix { device, inode } => {
        hasher.update(&[1]);
        hasher.update(&device.to_be_bytes());
        hasher.update(&inode.to_be_bytes());
        17
      },
      #[cfg(windows)]
      FileIdentity::Windows { volume, index } => {
        hasher.update(&[2]);
        hasher.update(&volume.to_be_bytes());
        hasher.update(&index.to_be_bytes());
        17
      },
      #[cfg(not(unix))]
      FileIdentity::Path(path) => {
        hasher.update(&[3]);
        let path = path.to_string_lossy();
        hasher.update(&(path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        9 + path.len() as u64
      },
    };
    hasher.update(&self.length.to_be_bytes());
    hasher.update(&[u8::from(self.modified.0)]);
    hasher.update(&self.modified.1.to_be_bytes());
    hasher.update(&self.modified.2.to_be_bytes());
    hasher.update(&self.change.0.to_be_bytes());
    hasher.update(&self.change.1.to_be_bytes());
    identity_bytes + 37
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
  use std::time::Duration;

  use super::*;

  #[test]
  fn timestamps_before_the_epoch_have_a_distinct_stable_key() {
    // Windows filesystem time has a coarser resolution than one nanosecond.
    // Whole seconds are exact on every supported platform while still testing
    // that the sign is part of the transient fingerprint.
    assert_eq!(system_time_key(UNIX_EPOCH - Duration::from_secs(1)), (true, 1, 0));
    assert_eq!(system_time_key(UNIX_EPOCH + Duration::from_secs(1)), (false, 1, 0));
  }

  #[cfg(any(target_os = "linux", target_os = "macos"))]
  #[test]
  fn memo_scope_is_stable_for_the_current_test_filesystem() {
    let directory = tempfile::tempdir().unwrap();
    let first = persistent_memo_scope(directory.path());
    let second = persistent_memo_scope(directory.path());
    assert_eq!(first, second);
    assert!(first.is_none_or(|scope| !scope.is_empty()));
  }

  #[cfg(any(target_os = "linux", target_os = "macos"))]
  #[test]
  fn memo_scope_distinguishes_filesystem_instances_and_boots() {
    let original = memo_scope(b"local", b"filesystem-a", b"boot-a");
    assert_ne!(original, memo_scope(b"local", b"filesystem-b", b"boot-a"));
    assert_ne!(original, memo_scope(b"local", b"filesystem-a", b"boot-b"));
  }

  #[cfg(windows)]
  #[test]
  fn unavailable_windows_file_ids_fall_back_to_the_exact_path() {
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first");
    let second_path = directory.path().join("second");
    std::fs::write(&first_path, b"first").unwrap();
    std::fs::write(&second_path, b"second").unwrap();
    let (first, _, first_persistent) = windows_fallback_identity(&first_path, &std::fs::metadata(&first_path).unwrap());
    let (second, _, second_persistent) =
      windows_fallback_identity(&second_path, &std::fs::metadata(&second_path).unwrap());
    assert_ne!(first, second);
    assert!(!first_persistent && !second_persistent);
  }

  #[cfg(windows)]
  #[test]
  fn reads_windows_file_identity_through_a_stable_handle_api() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("entry");
    std::fs::write(&path, b"cache input").unwrap();

    let (identity, change, persistent) = platform_identity(&path, &std::fs::metadata(&path).unwrap());
    assert!(matches!(identity, FileIdentity::Windows { .. }));
    assert_ne!(change, PlatformChange(0, 0));
    assert!(persistent);
  }
}
