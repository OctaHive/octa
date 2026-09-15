//! Shared local-cache capacity policy carried across process boundaries.

use serde::{Deserialize, Serialize};

use crate::CacheProtocolError;

/// Physical byte limits applied to one local cache scope.
///
/// Keeping the three related values together prevents config, agent, runner,
/// and cache implementations from accepting subtly different watermark
/// relationships.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCacheCapacity {
  /// Hard ceiling for the local cache scope.
  pub max_bytes: u64,
  /// Collection starts after physical usage reaches this value.
  pub high_watermark_bytes: u64,
  /// Collection attempts to reduce physical usage to this value.
  pub low_watermark_bytes: u64,
}

impl LocalCacheCapacity {
  /// Creates and validates one local-cache capacity policy.
  pub fn new(max_bytes: u64, high_watermark_bytes: u64, low_watermark_bytes: u64) -> Result<Self, CacheProtocolError> {
    let capacity = Self {
      max_bytes,
      high_watermark_bytes,
      low_watermark_bytes,
    };
    capacity.validate()?;
    Ok(capacity)
  }

  /// Validates `0 < low <= high <= max`.
  pub fn validate(&self) -> Result<(), CacheProtocolError> {
    if self.low_watermark_bytes == 0
      || self.low_watermark_bytes > self.high_watermark_bytes
      || self.high_watermark_bytes > self.max_bytes
    {
      return Err(CacheProtocolError::Configuration(
        "local cache capacity must satisfy 0 < low watermark <= high watermark <= maximum".to_owned(),
      ));
    }
    Ok(())
  }
}
