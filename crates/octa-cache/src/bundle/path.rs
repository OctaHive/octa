//! Output-root and symlink validation shared by packing and extraction.

use std::{cmp::Ordering, collections::HashSet, fs, path::Path};

use octa_cache_protocol::RelativePath;

use crate::{error::io_error, CacheError, CacheResult};

pub(crate) fn validate_output_roots(roots: &[RelativePath]) -> CacheResult<Vec<RelativePath>> {
  if roots.is_empty() {
    return Err(CacheError::Configuration(
      "at least one cache output root is required".to_owned(),
    ));
  }
  let mut roots = roots.to_vec();
  roots.sort_by(compare_paths);
  let mut seen = HashSet::<String>::with_capacity(roots.len());
  for root in &roots {
    if root.is_root() {
      return Err(CacheError::Configuration(
        "the workspace root cannot be a cache output".to_owned(),
      ));
    }
    if !seen.insert(root.as_str().to_owned()) {
      return Err(CacheError::Configuration(format!(
        "cache output root '{root}' is declared more than once"
      )));
    }
    // Check every segment ancestor instead of relying on sort adjacency. It
    // states the no-overlap invariant directly and remains correct if the
    // canonical ordering changes in a later format.
    for (separator, _) in root.as_str().match_indices('/') {
      let ancestor = &root.as_str()[..separator];
      if seen.contains(ancestor) {
        return Err(CacheError::Configuration(format!(
          "cache output roots '{ancestor}' and '{root}' overlap"
        )));
      }
    }
  }
  Ok(roots)
}

pub(super) fn contains(root: &RelativePath, path: &RelativePath) -> bool {
  path == root
    || path
      .as_str()
      .strip_prefix(root.as_str())
      .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Compares portable paths in deterministic tree-walk order.
///
/// Components are compared bytewise and an ancestor precedes every child.
/// Unlike comparison of the complete path string, this keeps an entire
/// directory subtree contiguous when a sibling contains punctuation that
/// sorts before `/`.
pub(super) fn compare_paths(left: &RelativePath, right: &RelativePath) -> Ordering {
  let mut left = left.as_str().split('/');
  let mut right = right.as_str().split('/');
  loop {
    match (left.next(), right.next()) {
      (Some(left), Some(right)) => match left.as_bytes().cmp(right.as_bytes()) {
        Ordering::Equal => {},
        ordering => return ordering,
      },
      (None, Some(_)) => return Ordering::Less,
      (Some(_), None) => return Ordering::Greater,
      (None, None) => return Ordering::Equal,
    }
  }
}

pub(super) fn check_empty_staging(staging: &Path) -> CacheResult<()> {
  if !staging.is_absolute() {
    return Err(CacheError::Configuration(
      "bundle staging directory must be absolute".to_owned(),
    ));
  }
  let metadata =
    fs::symlink_metadata(staging).map_err(|error| io_error("inspect bundle staging directory", staging, error))?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(CacheError::Configuration(
      "bundle staging path must be a real directory".to_owned(),
    ));
  }
  let mut entries = fs::read_dir(staging).map_err(|error| io_error("read bundle staging directory", staging, error))?;
  if entries
    .next()
    .transpose()
    .map_err(|error| io_error("read bundle staging entry", staging, error))?
    .is_some()
  {
    return Err(CacheError::Configuration(
      "bundle staging directory must be empty".to_owned(),
    ));
  }
  Ok(())
}

pub(super) fn prepare_entry_parent(path: &Path, output_root: bool) -> CacheResult<()> {
  let parent = path
    .parent()
    .ok_or_else(|| CacheError::InvalidBundle("bundle path has no parent".to_owned()))?;
  if output_root {
    return fs::create_dir_all(parent).map_err(|error| io_error("create staged output root parent", parent, error));
  }
  let metadata = match fs::symlink_metadata(parent) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      return Err(CacheError::InvalidBundle(
        "bundle entry has no explicit parent directory".to_owned(),
      ));
    },
    Err(error) => return Err(io_error("inspect staged output parent", parent, error)),
  };
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(CacheError::InvalidBundle(
      "bundle entry descends through a non-directory parent".to_owned(),
    ));
  }
  Ok(())
}
