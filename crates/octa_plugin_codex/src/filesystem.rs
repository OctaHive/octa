//! Cross-platform filesystem primitives used at Codex trust boundaries.
//!
//! Task code can mutate the workspace concurrently with finalization and
//! cleanup. Opening a previously inspected directory by name must therefore
//! refuse symbolic links and Windows reparse points in the open operation
//! itself, then verify the opened handle before callers use it.

use std::{fs::Metadata, io};

use cap_std::fs::{Dir, OpenOptions};

/// Creates and opens a portable relative directory path one component at a time.
///
/// An existing component is accepted only when it can be reopened by
/// [`open_directory_no_follow`]. This prevents a task-controlled workspace
/// from redirecting plugin-owned files through a pre-existing link.
pub(crate) fn create_directory_path_no_follow(root: &Dir, relative: &str) -> io::Result<Dir> {
  let mut directory = root.try_clone()?;
  for component in relative.split('/') {
    if component.is_empty() || matches!(component, "." | "..") {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "directory path contains a non-portable component",
      ));
    }
    match directory.create_dir(component) {
      Ok(()) => {},
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
      Err(error) => return Err(error),
    }
    directory = open_directory_no_follow(&directory, component)?;
  }
  Ok(directory)
}

/// Opens one child directory without following a final link or reparse point.
pub(crate) fn open_directory_no_follow(parent: &Dir, component: &str) -> io::Result<Dir> {
  let mut options = OpenOptions::new();
  options.read(true);
  configure_no_follow_directory_open(&mut options);
  let file = parent.open_with(component, &options)?.into_std();
  let metadata = file.metadata()?;
  if !metadata.is_dir() || is_std_link_or_reparse(&metadata) {
    return Err(io::Error::other("opened path is not a link-free directory"));
  }
  Ok(Dir::from_std_file(file))
}

/// Reports links that Rust exposes either directly or as Windows reparse data.
pub(crate) fn is_std_link_or_reparse(metadata: &Metadata) -> bool {
  if metadata.file_type().is_symlink() {
    return true;
  }
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
  }
  #[cfg(not(windows))]
  false
}

#[cfg(unix)]
fn configure_no_follow_directory_open(options: &mut OpenOptions) {
  use cap_std::fs::OpenOptionsExt as _;

  options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_no_follow_directory_open(options: &mut OpenOptions) {
  use cap_std::fs::OpenOptionsExt as _;
  use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT};

  options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow_directory_open(_options: &mut OpenOptions) {}
