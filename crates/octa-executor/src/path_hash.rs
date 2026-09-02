//! Platform-safe hashing of filesystem paths and their contents.

use std::{
  ffi::OsStr,
  fs::File,
  io::Read,
  path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::error::ExecutorResult;

pub(crate) fn content_fingerprint(paths: &[PathBuf], cancel_token: &CancellationToken) -> ExecutorResult<[u8; 32]> {
  check_cancelled(cancel_token)?;
  let mut hasher = Sha256::new();
  for path in paths {
    check_cancelled(cancel_token)?;
    hash_path(&mut hasher, path, cancel_token)?;
  }
  Ok(hasher.finalize().into())
}

pub(crate) fn update_path_identity(hasher: &mut Sha256, path: &Path) {
  update_os_string(hasher, path.as_os_str());
}

pub(crate) fn path_identity(path: &Path) -> String {
  let mut hasher = Sha256::new();
  update_path_identity(&mut hasher, path);
  hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash_path(hasher: &mut Sha256, path: &Path, cancel_token: &CancellationToken) -> ExecutorResult<()> {
  update_path_identity(hasher, path);

  let metadata = path.symlink_metadata()?;
  if metadata.file_type().is_symlink() {
    hasher.update([2]);
    update_os_string(hasher, path.read_link()?.as_os_str());
    return Ok(());
  }

  if metadata.is_dir() {
    hasher.update([0]);
    return Ok(());
  }

  hasher.update([1]);
  hasher.update(metadata.len().to_le_bytes());
  let mut file = File::open(path)?;
  let mut buffer = [0; 8192];
  loop {
    check_cancelled(cancel_token)?;
    let read = file.read(&mut buffer)?;
    if read == 0 {
      break;
    }
    hasher.update(&buffer[..read]);
  }
  Ok(())
}

fn check_cancelled(cancel_token: &CancellationToken) -> ExecutorResult<()> {
  if cancel_token.is_cancelled() {
    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "source hashing cancelled").into());
  }
  Ok(())
}

#[cfg(unix)]
fn update_os_string(hasher: &mut Sha256, value: &OsStr) {
  use std::os::unix::ffi::OsStrExt;

  let bytes = value.as_bytes();
  hasher.update((bytes.len() as u64).to_le_bytes());
  hasher.update(bytes);
}

#[cfg(windows)]
fn update_os_string(hasher: &mut Sha256, value: &OsStr) {
  use std::os::windows::ffi::OsStrExt;

  let units = value.encode_wide().collect::<Vec<_>>();
  hasher.update((units.len() as u64).to_le_bytes());
  for unit in units {
    hasher.update(unit.to_le_bytes());
  }
}

#[cfg(not(any(unix, windows)))]
fn update_os_string(hasher: &mut Sha256, value: &OsStr) {
  let value = value.to_string_lossy();
  hasher.update((value.len() as u64).to_le_bytes());
  hasher.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  #[test]
  fn content_and_path_are_both_part_of_the_fingerprint() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::write(&first, "same").unwrap();
    std::fs::write(&second, "same").unwrap();

    assert_ne!(path_identity(&first), path_identity(&second));
    assert_ne!(
      content_fingerprint(&[first], &CancellationToken::new()).unwrap(),
      content_fingerprint(&[second], &CancellationToken::new()).unwrap()
    );
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn non_utf8_paths_keep_their_native_identity() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let root = TempDir::new().unwrap();
    let first = root.path().join(OsString::from_vec(vec![b'a', 0x80]));
    let second = root.path().join(OsString::from_vec(vec![b'a', 0x81]));
    std::fs::write(&first, "same").unwrap();
    std::fs::write(&second, "same").unwrap();

    assert_ne!(path_identity(&first), path_identity(&second));
    assert_ne!(
      content_fingerprint(&[first], &CancellationToken::new()).unwrap(),
      content_fingerprint(&[second], &CancellationToken::new()).unwrap()
    );
  }
}
