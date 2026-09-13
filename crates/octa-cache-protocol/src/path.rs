//! Portable workspace-relative paths carried by cache metadata.

use std::{
  fmt,
  path::{Component, Path},
  str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{CacheProtocolError, MAX_CACHE_STRING_BYTES};

/// Normalized UTF-8 path relative to a workspace, using `/` separators.
///
/// `.` denotes the workspace root. Empty components, parent traversal,
/// backslashes, drive prefixes, and control characters are rejected so the
/// same serialized value has one meaning on Linux, macOS, and Windows.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RelativePath(String);

impl RelativePath {
  /// Validates and constructs a portable workspace-relative path.
  pub fn new(value: impl Into<String>) -> Result<Self, CacheProtocolError> {
    let value = value.into();
    validate(&value)?;
    Ok(Self(value))
  }

  /// Converts a host-relative path without changing the meaning of its components.
  ///
  /// `Path::components` recognizes Windows separators only on Windows. A
  /// backslash contained in a Unix filename therefore reaches [`Self::new`]
  /// unchanged and is rejected as non-portable instead of aliasing a `/` path.
  pub fn from_path(path: &Path) -> Result<Self, CacheProtocolError> {
    let mut components = Vec::new();
    for component in path.components() {
      match component {
        Component::Normal(value) => components.push(
          value
            .to_str()
            .ok_or_else(|| CacheProtocolError::Path("portable cache paths must be UTF-8".to_owned()))?,
        ),
        Component::CurDir if components.is_empty() => {},
        Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
          return Err(CacheProtocolError::Path(
            "host path must be normalized and relative".to_owned(),
          ));
        },
      }
    }
    if components.is_empty() {
      return Ok(Self::root());
    }
    Self::new(components.join("/"))
  }

  /// Constructs the distinguished path representing the workspace root.
  pub fn root() -> Self {
    Self(".".to_owned())
  }

  /// Returns the normalized `/`-separated wire representation.
  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Reports whether this value denotes the workspace root.
  pub fn is_root(&self) -> bool {
    self.0 == "."
  }

  /// Reports whether this path is equal to or nested below `root`.
  ///
  /// Both values are already normalized, so component containment can be
  /// checked without consulting a platform-specific filesystem.
  pub fn is_within(&self, root: &Self) -> bool {
    root.is_root()
      || self == root
      || self
        .as_str()
        .strip_prefix(root.as_str())
        .is_some_and(|suffix| suffix.starts_with('/'))
  }
}

impl fmt::Display for RelativePath {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl FromStr for RelativePath {
  type Err = CacheProtocolError;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    Self::new(value)
  }
}

impl Serialize for RelativePath {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&self.0)
  }
}

impl<'de> Deserialize<'de> for RelativePath {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = String::deserialize(deserializer)?;
    Self::new(value).map_err(serde::de::Error::custom)
  }
}

fn validate(value: &str) -> Result<(), CacheProtocolError> {
  if value.is_empty() {
    return Err(CacheProtocolError::Path(
      "path must not be empty; use '.' for the workspace root".to_owned(),
    ));
  }
  if value.len() > MAX_CACHE_STRING_BYTES {
    return Err(CacheProtocolError::Path(format!(
      "path exceeds {MAX_CACHE_STRING_BYTES} UTF-8 bytes"
    )));
  }
  if value == "." {
    return Ok(());
  }
  if value.starts_with('/') || value.contains(['\\', ':']) {
    return Err(CacheProtocolError::Path(
      "path must be portable, relative, and use '/' separators".to_owned(),
    ));
  }
  if value.chars().any(char::is_control) {
    return Err(CacheProtocolError::Path(
      "path must not contain control characters".to_owned(),
    ));
  }
  if value
    .split('/')
    .any(|part| part.is_empty() || part == "." || part == "..")
  {
    return Err(CacheProtocolError::Path(
      "path must be normalized and remain inside the workspace".to_owned(),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn converts_host_components_without_aliasing_distinct_unix_names() {
    assert_eq!(
      RelativePath::from_path(Path::new("nested/file")).unwrap().as_str(),
      "nested/file"
    );
    assert_eq!(RelativePath::from_path(Path::new("")).unwrap(), RelativePath::root());
    assert!(RelativePath::from_path(Path::new("../escape")).is_err());

    #[cfg(unix)]
    assert!(RelativePath::from_path(Path::new(r"nested\file")).is_err());
  }
}
