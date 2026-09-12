//! Deterministic streaming output bundles.
//!
//! The digest covers the canonical uncompressed byte stream. Compression is a
//! transfer detail, so changing the configured Zstandard level does not change
//! semantic content identity. Extraction accepts only an empty staging
//! directory; atomic replacement of live output roots belongs to the local
//! restore transaction built on top of this decoder.

mod extract;
mod pack;
mod path;

use octa_cache_protocol::{BlobDescriptor, BlobEncoding, Digest};

use crate::{
  workspace::{join_relative, portable_relative, safe_symlink_target, validate_symlink_text},
  CacheError, CacheResult,
};

pub use extract::extract_bundle;
pub use pack::{inspect_outputs, pack_bundle, PackedBundle};
pub(crate) use path::validate_output_roots;
use path::{check_empty_staging, compare_paths, contains, prepare_entry_parent};

/// Balanced Zstandard level used when a profile does not override compression.
pub const DEFAULT_BUNDLE_COMPRESSION_LEVEL: i32 = 3;

// These values begin the persisted format described in
// `docs/cache-formats-v1.md`. Packing and extraction deliberately share them;
// changing the magic, tags, field order, or integer encoding requires a new
// bundle version rather than an in-place change.
const BUNDLE_MAGIC: &[u8; 8] = b"OCTABND1";
const DIRECTORY_TAG: u8 = 1;
const FILE_TAG: u8 = 2;
const SYMLINK_TAG: u8 = 3;

/// Physical encoding selected while creating a bundle.
///
/// Compression is deliberately a producer option rather than part of
/// [`BundleLimits`], because extraction obtains the encoding from the trusted
/// result descriptor and must not validate an unused compression level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundleEncoding {
  /// Write the canonical stream without compression.
  Identity,
  /// Encode the canonical stream with Zstandard version one.
  ZstdV1 {
    /// Zstandard compression level accepted by this implementation.
    level: i32,
  },
}

impl Default for BundleEncoding {
  fn default() -> Self {
    Self::ZstdV1 {
      level: DEFAULT_BUNDLE_COMPRESSION_LEVEL,
    }
  }
}

impl BundleEncoding {
  /// Validates producer-side encoding parameters before a bundle is created.
  pub fn validate(self) -> CacheResult<Self> {
    if let Self::ZstdV1 { level } = self {
      if !(-7..=22).contains(&level) {
        return Err(CacheError::Configuration(
          "Zstandard level must be between -7 and 22".to_owned(),
        ));
      }
    }
    Ok(self)
  }

  fn protocol(self) -> BlobEncoding {
    match self {
      Self::Identity => BlobEncoding::Identity,
      Self::ZstdV1 { .. } => BlobEncoding::ZstdV1,
    }
  }
}

/// Central safety and resource bounds for bundle creation and extraction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BundleLimits {
  /// Maximum bytes accepted from or written to the encoded stream.
  pub max_encoded_bytes: u64,
  /// Maximum canonical bytes after decompression.
  pub max_expanded_bytes: u64,
  /// Maximum number of filesystem entries in one bundle.
  pub max_entries: u64,
  /// Maximum UTF-8 byte length of an entry's portable path.
  pub max_path_bytes: usize,
  /// Maximum content length of one regular file.
  pub max_file_bytes: u64,
  /// Maximum declared expanded-to-encoded size ratio.
  pub max_compression_ratio: u64,
  /// Reused streaming buffer size for file contents.
  pub read_buffer_bytes: usize,
}

impl Default for BundleLimits {
  fn default() -> Self {
    Self {
      max_encoded_bytes: 20 * 1024 * 1024 * 1024,
      max_expanded_bytes: 100 * 1024 * 1024 * 1024,
      max_entries: 1_000_000,
      max_path_bytes: 16 * 1024,
      max_file_bytes: 20 * 1024 * 1024 * 1024,
      max_compression_ratio: 1_000,
      read_buffer_bytes: 1024 * 1024,
    }
  }
}

impl BundleLimits {
  /// Validates that every configured resource bound is finite and usable.
  pub fn validate(self) -> CacheResult<Self> {
    if self.max_encoded_bytes == 0
      || self.max_expanded_bytes == 0
      || self.max_entries == 0
      || self.max_path_bytes == 0
      || self.max_file_bytes == 0
      || self.max_compression_ratio == 0
    {
      return Err(CacheError::Configuration(
        "bundle size, entry, path, file, and compression-ratio limits must be nonzero".to_owned(),
      ));
    }
    if !(4 * 1024..=16 * 1024 * 1024).contains(&self.read_buffer_bytes) {
      return Err(CacheError::Configuration(
        "bundle read_buffer_bytes must be between 4 KiB and 16 MiB".to_owned(),
      ));
    }
    Ok(self)
  }

  /// Rejects a bundle descriptor before its encoded body is downloaded.
  ///
  /// Stores validate the protocol shape of action metadata, while the caller
  /// selects deployment-specific byte, entry, and compression-ratio limits.
  /// Keeping this check public lets remote consumers enforce those local
  /// limits before allocating temporary disk space or reading an untrusted
  /// stream. Extraction repeats the same validation as defense in depth.
  pub fn validate_descriptor(&self, descriptor: &BlobDescriptor) -> CacheResult<()> {
    self.validate()?;
    descriptor.validate()?;
    validate_descriptor_limits(descriptor, *self)
  }
}

fn descriptor(
  digest: Digest,
  encoding: BlobEncoding,
  encoded_size_bytes: u64,
  expanded_size_bytes: u64,
  entry_count: u64,
) -> BlobDescriptor {
  BlobDescriptor {
    digest,
    encoding,
    encoded_size_bytes,
    expanded_size_bytes,
    entry_count,
  }
}

fn validate_descriptor_limits(descriptor: &BlobDescriptor, limits: BundleLimits) -> CacheResult<()> {
  if descriptor.encoded_size_bytes > limits.max_encoded_bytes
    || descriptor.expanded_size_bytes > limits.max_expanded_bytes
    || descriptor.entry_count > limits.max_entries
  {
    return Err(CacheError::Limit(
      "bundle descriptor exceeds configured size or entry limits".to_owned(),
    ));
  }
  let allowed_expanded = descriptor
    .encoded_size_bytes
    .saturating_mul(limits.max_compression_ratio);
  if descriptor.expanded_size_bytes > allowed_expanded {
    return Err(CacheError::Limit(
      "bundle exceeds the configured compression ratio".to_owned(),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests;
