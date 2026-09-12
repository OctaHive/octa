//! Shared workspace-relative path and symlink safety rules.
//!
//! Input discovery, bundle creation, bundle extraction, and restore must agree
//! on one portable path model. Keeping the lexical rules here prevents a path
//! accepted by one stage from being interpreted differently by another.

use std::path::{Component, Path, PathBuf};

use octa_cache_protocol::RelativePath;

use crate::{error::io_error, CacheError, CacheResult};

/// Converts an already-contained host path into its portable cache form.
pub(crate) fn portable_relative(workspace: &Path, path: &Path) -> CacheResult<RelativePath> {
  let relative = path.strip_prefix(workspace).map_err(|_| CacheError::Path {
    path: path.to_path_buf(),
    reason: "path escaped the workspace".to_owned(),
  })?;
  let value = relative.to_str().ok_or_else(|| CacheError::Path {
    path: relative.to_path_buf(),
    reason: "portable cache paths must be UTF-8".to_owned(),
  })?;
  RelativePath::new(value.replace('\\', "/")).map_err(CacheError::from)
}

/// Joins a validated `/`-separated path without host-dependent parsing.
pub(crate) fn join_relative(root: &Path, path: &RelativePath) -> PathBuf {
  let mut result = root.to_path_buf();
  for component in path.as_str().split('/') {
    result.push(component);
  }
  result
}

/// Validates portable symlink text against its workspace-relative link path.
///
/// This is lexical and therefore usable before extraction creates the target.
/// Callers inspecting a live tree must additionally canonicalize the resolved
/// target with [`safe_symlink_target`].
pub(crate) fn validate_symlink_text(link: &RelativePath, target: &str) -> Result<(), String> {
  if target.is_empty()
    || target.starts_with('/')
    || target.contains(['\\', ':'])
    || target.chars().any(char::is_control)
  {
    return Err(format!("symlink '{link}' has a non-portable target"));
  }
  let mut depth = link.as_str().split('/').count().saturating_sub(1);
  for component in Path::new(target).components() {
    match component {
      Component::CurDir => {},
      Component::Normal(_) => depth += 1,
      Component::ParentDir if depth > 0 => depth -= 1,
      Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
        return Err(format!("symlink '{link}' escapes the workspace"));
      },
    }
  }
  Ok(())
}

/// Returns portable target text after proving the live target remains contained.
pub(crate) fn safe_symlink_target(workspace: &Path, link: &Path, target: &Path) -> CacheResult<String> {
  let target_text = target.to_str().ok_or_else(|| CacheError::Path {
    path: target.to_path_buf(),
    reason: "portable symlink targets must be UTF-8".to_owned(),
  })?;
  let relative_link = portable_relative(workspace, link)?;
  validate_symlink_text(&relative_link, target_text).map_err(|reason| CacheError::Path {
    path: link.to_path_buf(),
    reason,
  })?;
  let resolved = dunce::canonicalize(
    link
      .parent()
      .expect("a workspace-relative link has a workspace parent")
      .join(target),
  )
  .map_err(|error| io_error("resolve cache symlink", link, error))?;
  if !resolved.starts_with(workspace) {
    return Err(CacheError::Path {
      path: link.to_path_buf(),
      reason: "symlink target resolves outside the workspace".to_owned(),
    });
  }
  Ok(target_text.to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn portable_symlink_rules_reject_every_escape_form() {
    let link = RelativePath::new("nested/link").unwrap();
    for valid in ["target", "./target", "../target"] {
      assert!(validate_symlink_text(&link, valid).is_ok(), "{valid}");
    }
    for invalid in ["", "/absolute", "C:/drive", "..\\target", "../../escape", "bad\nname"] {
      assert!(validate_symlink_text(&link, invalid).is_err(), "{invalid}");
    }
  }
}
