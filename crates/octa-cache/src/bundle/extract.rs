//! Bounded verification and extraction into an empty staging tree.

use std::{
  cmp::Ordering,
  fs::{self, File},
  io::{self, Read, Write},
  path::Path,
};

use octa_cache_protocol::{
  BlobDescriptor, BlobEncoding, Digest, DigestAlgorithm, RelativePath, ZSTD_V1_MAX_WINDOW_LOG,
};
use tokio_util::sync::CancellationToken;

use crate::{
  error::{check_cancelled, io_error},
  platform::{create_symlink, set_executable},
  CacheError, CacheResult,
};

use super::{
  check_empty_staging, compare_paths, contains, join_relative, prepare_entry_parent, validate_descriptor_limits,
  validate_output_roots, validate_symlink_text, BundleLimits, BUNDLE_MAGIC, DIRECTORY_TAG, FILE_TAG, SYMLINK_TAG,
};

/// Verifies and extracts a bundle into an existing empty staging directory.
///
/// This function never replaces live task outputs. The caller owns the staging
/// directory and is responsible for the later atomic restore transaction.
///
/// # Errors
///
/// Returns an error when the descriptor or canonical stream is invalid, a
/// resource bound is exceeded, staging is unsafe, I/O fails, or cancellation
/// is requested. Partially staged files may remain after an error.
pub fn extract_bundle<R: Read>(
  reader: R,
  descriptor: &BlobDescriptor,
  staging: &Path,
  output_roots: &[RelativePath],
  limits: BundleLimits,
  cancel: &CancellationToken,
) -> CacheResult<()> {
  let limits = limits.validate()?;
  descriptor.validate()?;
  validate_descriptor_limits(descriptor, limits)?;
  check_empty_staging(staging)?;
  let roots = validate_output_roots(output_roots)?;
  let mut encoded = CountingReader::new(reader, descriptor.encoded_size_bytes);
  match descriptor.encoding {
    BlobEncoding::Identity => extract_canonical(&mut encoded, descriptor, staging, &roots, limits, cancel)?,
    BlobEncoding::ZstdV1 => {
      let mut decoder = zstd::stream::read::Decoder::new(&mut encoded).map_err(CacheError::Stream)?;
      decoder
        .window_log_max(ZSTD_V1_MAX_WINDOW_LOG)
        .map_err(CacheError::Stream)?;
      extract_canonical(decoder, descriptor, staging, &roots, limits, cancel)?;
    },
  }
  encoded.verify_complete()
}
fn extract_canonical<R: Read>(
  reader: R,
  descriptor: &BlobDescriptor,
  staging: &Path,
  roots: &[RelativePath],
  limits: BundleLimits,
  cancel: &CancellationToken,
) -> CacheResult<()> {
  // Decode and hash the same canonical stream emitted by `write_canonical`.
  // Strict tree order makes duplicate, missing, and out-of-contract roots
  // detectable without retaining a set of every decoded path.
  let mut reader = CanonicalReader::new(reader, limits.max_expanded_bytes);
  if reader.read_array::<8>()? != *BUNDLE_MAGIC {
    return Err(CacheError::InvalidBundle("unknown bundle magic or version".to_owned()));
  }
  let mut previous = None;
  let mut current_root = None;
  let mut buffer = vec![0_u8; limits.read_buffer_bytes];
  for _ in 0..descriptor.entry_count {
    check_cancelled(cancel)?;
    let tag = reader.read_u8()?;
    let path = reader.read_relative_path(limits.max_path_bytes)?;
    if previous
      .as_ref()
      .is_some_and(|previous| compare_paths(previous, &path) != Ordering::Less)
    {
      return Err(CacheError::InvalidBundle(
        "bundle paths must be unique and follow canonical tree order".to_owned(),
      ));
    }
    let output_root = match current_root {
      None => {
        if roots.first() != Some(&path) {
          return Err(CacheError::InvalidBundle(format!(
            "bundle does not begin declared output root '{}'",
            roots[0]
          )));
        }
        current_root = Some(0);
        true
      },
      Some(index) if contains(&roots[index], &path) => false,
      Some(index) => {
        let next = index + 1;
        let next_root = roots
          .get(next)
          .ok_or_else(|| CacheError::InvalidBundle(format!("bundle path '{path}' is outside declared output roots")))?;
        if &path != next_root {
          return Err(CacheError::InvalidBundle(format!(
            "bundle does not begin declared output root '{next_root}'"
          )));
        }
        current_root = Some(next);
        true
      },
    };
    let destination = join_relative(staging, &path);
    prepare_entry_parent(&destination, output_root)?;
    match tag {
      DIRECTORY_TAG => {
        fs::create_dir(&destination).map_err(|error| io_error("create staged output directory", &destination, error))?
      },
      FILE_TAG => {
        let executable = reader.read_bool()?;
        let length = reader.read_u64()?;
        if length > limits.max_file_bytes {
          return Err(CacheError::Limit(format!(
            "bundle file '{path}' exceeds the configured limit"
          )));
        }
        let mut file =
          File::create(&destination).map_err(|error| io_error("create staged output file", &destination, error))?;
        reader.copy_exact(&mut file, length, &mut buffer, cancel)?;
        file
          .sync_all()
          .map_err(|error| io_error("synchronize staged output file", &destination, error))?;
        set_executable(&destination, executable)
          .map_err(|error| io_error("set staged output permissions", &destination, error))?;
      },
      SYMLINK_TAG => {
        let directory = reader.read_bool()?;
        let target = reader.read_symlink_target(&path, limits.max_path_bytes)?;
        create_symlink(Path::new(&target), &destination, directory)
          .map_err(|error| io_error("create staged output symlink", &destination, error))?;
      },
      _ => return Err(CacheError::InvalidBundle(format!("unknown bundle entry tag {tag}"))),
    }
    previous = Some(path);
  }
  let completed_roots = current_root.map_or(0, |index| index + 1);
  if completed_roots != roots.len() {
    let root = &roots[completed_roots];
    return Err(CacheError::InvalidBundle(format!(
      "bundle does not contain declared output root '{root}'"
    )));
  }
  if reader.read_optional_byte()?.is_some() {
    return Err(CacheError::InvalidBundle(
      "bundle has trailing canonical bytes".to_owned(),
    ));
  }
  let (actual, expanded) = reader.finish();
  if expanded != descriptor.expanded_size_bytes || actual != descriptor.digest {
    return Err(CacheError::InvalidBundle(
      "bundle expanded size or content digest differs from its descriptor".to_owned(),
    ));
  }
  Ok(())
}

/// Bounded reader that hashes every expanded byte consumed by the decoder.
struct CanonicalReader<R> {
  inner: R,
  hasher: blake3::Hasher,
  bytes: u64,
  maximum: u64,
}

impl<R: Read> CanonicalReader<R> {
  fn new(inner: R, maximum: u64) -> Self {
    Self {
      inner,
      hasher: blake3::Hasher::new(),
      bytes: 0,
      maximum,
    }
  }

  fn read_exact(&mut self, buffer: &mut [u8]) -> CacheResult<()> {
    let next = self
      .bytes
      .checked_add(buffer.len() as u64)
      .ok_or_else(|| CacheError::Limit("expanded bundle size overflowed".to_owned()))?;
    if next > self.maximum {
      return Err(CacheError::Limit("expanded bundle exceeds configured limit".to_owned()));
    }
    self.inner.read_exact(buffer).map_err(CacheError::Stream)?;
    self.hasher.update(buffer);
    self.bytes = next;
    Ok(())
  }

  fn read_array<const N: usize>(&mut self) -> CacheResult<[u8; N]> {
    let mut bytes = [0_u8; N];
    self.read_exact(&mut bytes)?;
    Ok(bytes)
  }

  fn read_u8(&mut self) -> CacheResult<u8> {
    Ok(self.read_array::<1>()?[0])
  }

  fn read_bool(&mut self) -> CacheResult<bool> {
    match self.read_u8()? {
      0 => Ok(false),
      1 => Ok(true),
      value => Err(CacheError::InvalidBundle(format!("invalid bundle boolean {value}"))),
    }
  }

  fn read_u32(&mut self) -> CacheResult<u32> {
    Ok(u32::from_be_bytes(self.read_array()?))
  }

  fn read_u64(&mut self) -> CacheResult<u64> {
    Ok(u64::from_be_bytes(self.read_array()?))
  }

  fn read_relative_path(&mut self, maximum: usize) -> CacheResult<RelativePath> {
    let value = self.read_string(maximum)?;
    let path = RelativePath::new(value)?;
    if path.is_root() {
      return Err(CacheError::InvalidBundle(
        "bundle entries cannot replace the workspace root".to_owned(),
      ));
    }
    Ok(path)
  }

  fn read_symlink_target(&mut self, link: &RelativePath, maximum: usize) -> CacheResult<String> {
    let target = self.read_string(maximum)?;
    validate_symlink_text(link, &target).map_err(CacheError::InvalidBundle)?;
    Ok(target)
  }

  fn read_string(&mut self, maximum: usize) -> CacheResult<String> {
    let length = self.read_u32()? as usize;
    if length == 0 || length > maximum {
      return Err(CacheError::Limit(format!(
        "bundle string length {length} is outside configured bounds"
      )));
    }
    let mut bytes = vec![0_u8; length];
    self.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| CacheError::InvalidBundle("bundle paths must be UTF-8".to_owned()))
  }

  fn copy_exact<W: Write>(
    &mut self,
    writer: &mut W,
    mut remaining: u64,
    buffer: &mut [u8],
    cancel: &CancellationToken,
  ) -> CacheResult<()> {
    while remaining > 0 {
      check_cancelled(cancel)?;
      let length = remaining.min(buffer.len() as u64) as usize;
      self.read_exact(&mut buffer[..length])?;
      writer.write_all(&buffer[..length]).map_err(CacheError::Stream)?;
      remaining -= length as u64;
    }
    Ok(())
  }

  fn read_optional_byte(&mut self) -> CacheResult<Option<u8>> {
    let mut byte = [0_u8; 1];
    match self.inner.read(&mut byte) {
      Ok(0) => Ok(None),
      Ok(read) => {
        debug_assert_eq!(read, 1, "Read cannot fill more than its one-byte buffer");
        self.hasher.update(&byte);
        self.bytes += 1;
        Ok(Some(byte[0]))
      },
      Err(source) => Err(CacheError::Stream(source)),
    }
  }

  fn finish(self) -> (Digest, u64) {
    (
      Digest::new(DigestAlgorithm::Blake3, *self.hasher.finalize().as_bytes(), self.bytes),
      self.bytes,
    )
  }
}

/// Restricts reads to the descriptor's physical size and rejects trailing bytes.
struct CountingReader<R> {
  inner: R,
  bytes: u64,
  maximum: u64,
}

impl<R> CountingReader<R> {
  fn new(inner: R, maximum: u64) -> Self {
    Self {
      inner,
      bytes: 0,
      maximum,
    }
  }

  fn verify_complete(&self) -> CacheResult<()> {
    if self.bytes != self.maximum {
      return Err(CacheError::InvalidBundle(format!(
        "bundle encoded size is {}, expected {}",
        self.bytes, self.maximum
      )));
    }
    Ok(())
  }
}

impl<R: Read> Read for CountingReader<R> {
  fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
    let remaining = self.maximum.saturating_sub(self.bytes);
    if remaining == 0 && !buffer.is_empty() {
      let mut extra = [0_u8; 1];
      return match self.inner.read(&mut extra)? {
        0 => Ok(0),
        _ => Err(io::Error::other("encoded bundle exceeds descriptor size")),
      };
    }
    let allowed = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
    let read = self.inner.read(&mut buffer[..allowed])?;
    self.bytes += read as u64;
    Ok(read)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct ErrorReader;

  impl Read for ErrorReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
      Err(io::Error::other("fixture read failure"))
    }
  }

  #[test]
  fn canonical_reader_enforces_expanded_bounds_and_surfaces_tail_errors() {
    let mut bounded = CanonicalReader::new(&b"ab"[..], 1);
    assert!(matches!(bounded.read_array::<2>(), Err(CacheError::Limit(_))));

    let mut failing = CanonicalReader::new(ErrorReader, 1);
    assert!(matches!(failing.read_optional_byte(), Err(CacheError::Stream(_))));
  }

  #[test]
  fn counting_reader_rejects_bytes_past_the_descriptor() {
    let mut reader = CountingReader::new(&b"ab"[..], 1);
    let mut byte = [0_u8; 1];
    assert_eq!(reader.read(&mut byte).unwrap(), 1);
    assert!(reader.read(&mut byte).is_err());

    let incomplete = CountingReader::new(&b"a"[..], 2);
    assert!(matches!(
      incomplete.verify_complete(),
      Err(CacheError::InvalidBundle(_))
    ));
  }
}
