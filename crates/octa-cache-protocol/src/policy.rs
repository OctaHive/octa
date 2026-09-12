//! Transport-stable cache access policy shared by profiles and runner sessions.

use serde::{Deserialize, Serialize};

/// Read and immutable-publication permissions for one cache session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
  /// Restore existing action results without publishing new ones.
  ReadOnly,
  /// Publish successful actions without consulting existing results.
  WriteOnly,
  /// Restore and publish results.
  ReadWrite,
}

impl CacheMode {
  /// Whether this session may consult existing action records.
  pub const fn can_read(self) -> bool {
    matches!(self, Self::ReadOnly | Self::ReadWrite)
  }

  /// Whether this session may publish immutable blobs and action records.
  pub const fn can_write(self) -> bool {
    matches!(self, Self::WriteOnly | Self::ReadWrite)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn preserves_one_canonical_spelling_and_independent_permissions() {
    assert_eq!(serde_json::to_string(&CacheMode::ReadWrite).unwrap(), r#""read_write""#);
    assert_eq!(
      serde_json::from_str::<CacheMode>(r#""read_write""#).unwrap(),
      CacheMode::ReadWrite
    );
    assert!(serde_json::from_str::<CacheMode>(r#""read-write""#).is_err());
    assert!(CacheMode::ReadOnly.can_read());
    assert!(!CacheMode::ReadOnly.can_write());
    assert!(!CacheMode::WriteOnly.can_read());
    assert!(CacheMode::WriteOnly.can_write());
  }
}
