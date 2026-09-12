//! Output discovery and deterministic streaming bundle creation.

use std::{
  fs::{self, File},
  io::{self, Read, Write},
  path::{Path, PathBuf},
};

use octa_cache_protocol::{BlobDescriptor, Digest, DigestAlgorithm, RelativePath, ZSTD_V1_MAX_WINDOW_LOG};
use tokio_util::sync::CancellationToken;

use crate::{
  error::{check_cancelled, io_error},
  platform::{executable, EntryKey},
  CacheError, CacheResult,
};

use super::{
  descriptor, join_relative, portable_relative, safe_symlink_target, validate_output_roots, BundleEncoding,
  BundleLimits, BUNDLE_MAGIC, DIRECTORY_TAG, FILE_TAG, SYMLINK_TAG,
};

/// Writer returned together with metadata needed by CAS publication.
pub struct PackedBundle<W> {
  /// Original sink, returned after all bytes have been flushed.
  pub writer: W,
  /// Canonical content identity and encoded-stream metadata.
  pub descriptor: BlobDescriptor,
}

/// Packs exact workspace-relative output roots into a deterministic stream.
///
/// The output tree must remain quiescent while it is packed. Octa validates
/// every entry against the metadata observed during discovery and rejects a
/// tree that changes before publication. File bytes are streamed, and only
/// the sorted child lists belonging to the active traversal stack are held in
/// memory.
///
/// # Errors
///
/// Returns an error when an output is missing, unsafe, unsupported, unstable,
/// exceeds a configured bound, cannot be read, or cancellation is requested.
pub fn pack_bundle<W: Write>(
  writer: W,
  workspace: &Path,
  output_roots: &[RelativePath],
  encoding: BundleEncoding,
  limits: BundleLimits,
  cancel: &CancellationToken,
) -> CacheResult<PackedBundle<W>> {
  let limits = limits.validate()?;
  let encoding = encoding.validate()?;
  check_cancelled(cancel)?;
  let workspace =
    dunce::canonicalize(workspace).map_err(|error| io_error("canonicalize bundle workspace", workspace, error))?;
  let roots = validate_output_roots(output_roots)?;
  match encoding {
    BundleEncoding::Identity => {
      let mut encoded = CountingWriter::new(writer, limits.max_encoded_bytes);
      let (digest, expanded, entry_count) = write_canonical(&mut encoded, &workspace, &roots, limits, cancel)?;
      encoded.flush().map_err(CacheError::Stream)?;
      let encoded_size = encoded.bytes;
      Ok(PackedBundle {
        writer: encoded.inner,
        descriptor: descriptor(digest, encoding.protocol(), encoded_size, expanded, entry_count),
      })
    },
    BundleEncoding::ZstdV1 { level } => {
      let encoded = CountingWriter::new(writer, limits.max_encoded_bytes);
      let mut encoder = zstd::stream::write::Encoder::new(encoded, level).map_err(CacheError::Stream)?;
      encoder.window_log(ZSTD_V1_MAX_WINDOW_LOG).map_err(CacheError::Stream)?;
      let (digest, expanded, entry_count) = write_canonical(&mut encoder, &workspace, &roots, limits, cancel)?;
      let encoded = encoder.finish().map_err(CacheError::Stream)?;
      let mut encoded = encoded;
      encoded.flush().map_err(CacheError::Stream)?;
      let encoded_size = encoded.bytes;
      Ok(PackedBundle {
        writer: encoded.inner,
        descriptor: descriptor(digest, encoding.protocol(), encoded_size, expanded, entry_count),
      })
    },
  }
}

/// Computes the canonical identity of currently materialized output roots.
///
/// Inspection deliberately uses the bundle writer with an identity sink rather
/// than maintaining a second output-tree hashing implementation. Consequently,
/// the comparison made before restore observes exactly the paths, entry kinds,
/// executable bits, symlinks, and file bytes that bundle publication captures.
/// The returned descriptor describes the discarded canonical stream; callers
/// compare its digest, expanded size, and entry count with a stored bundle.
pub fn inspect_outputs(
  workspace: &Path,
  output_roots: &[RelativePath],
  limits: BundleLimits,
  cancel: &CancellationToken,
) -> CacheResult<BlobDescriptor> {
  Ok(
    pack_bundle(
      io::sink(),
      workspace,
      output_roots,
      BundleEncoding::Identity,
      limits,
      cancel,
    )?
    .descriptor,
  )
}

struct OutputEntry {
  path: RelativePath,
  source: PathBuf,
  discovered: EntryKey,
  kind: OutputKind,
}

enum OutputKind {
  Directory,
  File { length: u64, executable: bool },
  Symlink { target: String, directory: bool },
}

/// Walks roots in the same component-wise depth-first order required on wire.
///
/// The visitor sees one transient entry at a time. Directory children are
/// sorted locally, avoiding a process-sized table for the complete output.
fn visit_output_tree(
  workspace: &Path,
  roots: &[RelativePath],
  limits: BundleLimits,
  cancel: &CancellationToken,
  visit: &mut impl FnMut(&OutputEntry) -> CacheResult<()>,
) -> CacheResult<u64> {
  enum Frame {
    Enter(PathBuf),
    ResumeDirectory {
      entry: OutputEntry,
      discovered_children: Vec<PathBuf>,
      next_child: usize,
      child_limit: u64,
    },
  }

  let root_count = u64::try_from(roots.len())
    .map_err(|_| CacheError::Limit("output root count does not fit the bundle format".to_owned()))?;
  if root_count > limits.max_entries {
    return Err(CacheError::Limit(format!(
      "output bundle contains more than {} entries",
      limits.max_entries
    )));
  }
  let mut entries = 0_u64;
  // Count paths when they are scheduled, not when they are visited. This
  // prevents a deep directory from consuming a budget already reserved by
  // siblings waiting in the traversal.
  let mut scheduled = root_count;
  let mut stack = roots
    .iter()
    .rev()
    .map(|root| Frame::Enter(join_relative(workspace, root)))
    .collect::<Vec<_>>();
  while let Some(frame) = stack.pop() {
    match frame {
      Frame::Enter(path) => {
        check_cancelled(cancel)?;
        let entry = inspect_output_entry(workspace, &path, limits)?;
        entries += 1;
        visit(&entry)?;
        if matches!(entry.kind, OutputKind::Directory) {
          let child_limit = limits.max_entries - scheduled;
          let children = read_sorted_children(&path, child_limit)?;
          scheduled += children.len() as u64;
          stack.push(Frame::ResumeDirectory {
            entry,
            discovered_children: children,
            next_child: 0,
            child_limit,
          });
        } else {
          validate_stable_entry(&entry)?;
        }
      },
      Frame::ResumeDirectory {
        entry,
        discovered_children,
        next_child,
        child_limit,
      } => {
        if let Some(child) = discovered_children.get(next_child).cloned() {
          // Retain one child list per active directory and schedule only the
          // next path. This avoids cloning every child into a second stack.
          stack.push(Frame::ResumeDirectory {
            entry,
            discovered_children,
            next_child: next_child + 1,
            child_limit,
          });
          stack.push(Frame::Enter(child));
        } else {
          // Do not trust directory timestamps alone: a child may be added and
          // removed between coarse timestamp ticks on some filesystems.
          if read_sorted_children(&entry.source, child_limit)? != discovered_children {
            return Err(CacheError::UnstableFile { path: entry.source });
          }
          validate_stable_entry(&entry)?;
        }
      },
    }
  }
  Ok(entries)
}

fn inspect_output_entry(workspace: &Path, path: &Path, limits: BundleLimits) -> CacheResult<OutputEntry> {
  let metadata = fs::symlink_metadata(path).map_err(|error| io_error("inspect declared output", path, error))?;
  let relative = portable_relative(workspace, path)?;
  if relative.as_str().len() > limits.max_path_bytes {
    return Err(CacheError::Limit(format!(
      "output path exceeds {} UTF-8 bytes",
      limits.max_path_bytes
    )));
  }
  let file_type = metadata.file_type();
  let discovered = EntryKey::new(path, &metadata)?;
  let kind = if file_type.is_symlink() {
    let target = fs::read_link(path).map_err(|error| io_error("read output symlink", path, error))?;
    let target = safe_symlink_target(workspace, path, &target)?;
    let followed = fs::metadata(path).map_err(|error| io_error("inspect output symlink target", path, error))?;
    OutputKind::Symlink {
      target,
      directory: followed.is_dir(),
    }
  } else if metadata.is_dir() {
    OutputKind::Directory
  } else if metadata.is_file() {
    if metadata.len() > limits.max_file_bytes {
      return Err(CacheError::Limit(format!(
        "output file '{}' exceeds {} bytes",
        relative, limits.max_file_bytes
      )));
    }
    OutputKind::File {
      length: metadata.len(),
      executable: executable(&metadata),
    }
  } else {
    return Err(CacheError::UnsupportedEntry {
      path: path.to_path_buf(),
      kind: "only regular files, directories, and symlinks can be bundled",
    });
  };
  Ok(OutputEntry {
    path: relative,
    source: path.to_path_buf(),
    discovered,
    kind,
  })
}

fn read_sorted_children(path: &Path, maximum: u64) -> CacheResult<Vec<PathBuf>> {
  let mut children = Vec::new();
  for entry in fs::read_dir(path).map_err(|error| io_error("read output directory", path, error))? {
    if children.len() as u64 >= maximum {
      return Err(CacheError::Limit(format!(
        "output directory '{}' exceeds the remaining bundle entry limit",
        path.display()
      )));
    }
    let entry = entry.map_err(|error| io_error("read output directory entry", path, error))?;
    let name = entry.file_name().into_string().map_err(|name| CacheError::Path {
      path: path.join(name),
      reason: "portable cache paths must be UTF-8".to_owned(),
    })?;
    children.push((name, entry.path()));
  }
  children.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
  Ok(children.into_iter().map(|(_, path)| path).collect())
}

/// Compact metadata generation used to reject cross-generation bundles.
#[derive(Debug, Eq, PartialEq)]
struct TreeSummary {
  entries: u64,
  fingerprint: [u8; 32],
}

struct TreeSummaryBuilder {
  entries: u64,
  hasher: blake3::Hasher,
}

impl TreeSummaryBuilder {
  fn new() -> Self {
    Self {
      entries: 0,
      hasher: blake3::Hasher::new(),
    }
  }

  fn observe(&mut self, entry: &OutputEntry) {
    self.entries += 1;
    let path = entry.path.as_str().as_bytes();
    self.hasher.update(&(path.len() as u32).to_be_bytes());
    self.hasher.update(path);
    match &entry.kind {
      OutputKind::Directory => {
        self.hasher.update(&[DIRECTORY_TAG]);
      },
      OutputKind::File { length, executable } => {
        self.hasher.update(&[FILE_TAG, u8::from(*executable)]);
        self.hasher.update(&length.to_be_bytes());
      },
      OutputKind::Symlink { target, directory } => {
        self.hasher.update(&[SYMLINK_TAG, u8::from(*directory)]);
        self.hasher.update(&(target.len() as u32).to_be_bytes());
        self.hasher.update(target.as_bytes());
      },
    };
    entry.discovered.update_fingerprint(&mut self.hasher);
  }

  fn finish(self) -> TreeSummary {
    TreeSummary {
      entries: self.entries,
      fingerprint: *self.hasher.finalize().as_bytes(),
    }
  }
}

fn summarize_output_tree(
  workspace: &Path,
  roots: &[RelativePath],
  limits: BundleLimits,
  cancel: &CancellationToken,
) -> CacheResult<TreeSummary> {
  let mut summary = TreeSummaryBuilder::new();
  visit_output_tree(workspace, roots, limits, cancel, &mut |entry| {
    summary.observe(entry);
    Ok(())
  })?;
  Ok(summary.finish())
}

fn write_canonical<W: Write>(
  writer: &mut W,
  workspace: &Path,
  roots: &[RelativePath],
  limits: BundleLimits,
  cancel: &CancellationToken,
) -> CacheResult<(Digest, u64, u64)> {
  // `CanonicalWriter` hashes the exact uncompressed bytes it forwards. The
  // transport encoder, when present, wraps this writer outside this function;
  // compression settings therefore cannot change semantic bundle identity.
  let mut writer = CanonicalWriter::new(writer, limits.max_expanded_bytes);
  writer.write_all(BUNDLE_MAGIC)?;
  let mut buffer = vec![0_u8; limits.read_buffer_bytes];
  let mut summary = TreeSummaryBuilder::new();
  visit_output_tree(workspace, roots, limits, cancel, &mut |entry| {
    summary.observe(entry);
    let path = entry.path.as_str().as_bytes();
    match &entry.kind {
      OutputKind::Directory => writer.write_all(&[DIRECTORY_TAG])?,
      OutputKind::File { .. } => writer.write_all(&[FILE_TAG])?,
      OutputKind::Symlink { .. } => writer.write_all(&[SYMLINK_TAG])?,
    }
    writer.write_all(&(path.len() as u32).to_be_bytes())?;
    writer.write_all(path)?;
    match &entry.kind {
      OutputKind::Directory => {},
      OutputKind::File { length, executable } => {
        writer.write_all(&[u8::from(*executable)])?;
        writer.write_all(&length.to_be_bytes())?;
        copy_stable_file(entry, *length, &mut buffer, &mut writer, cancel)?;
      },
      OutputKind::Symlink { target, directory } => {
        writer.write_all(&[u8::from(*directory)])?;
        writer.write_all(&(target.len() as u32).to_be_bytes())?;
        writer.write_all(target.as_bytes())?;
      },
    }
    Ok(())
  })?;
  let written = summary.finish();
  let canonical = writer.finish()?;
  // Earlier roots can change while later roots are copied. A final metadata
  // walk compares a compact fingerprint rather than retaining every entry.
  if summarize_output_tree(workspace, roots, limits, cancel)? != written {
    return Err(CacheError::UnstableFile {
      path: workspace.to_path_buf(),
    });
  }
  Ok((canonical.0, canonical.1, written.entries))
}

fn validate_stable_entry(entry: &OutputEntry) -> CacheResult<()> {
  let metadata = fs::symlink_metadata(&entry.source)
    .map_err(|error| io_error("reinspect output while bundling", &entry.source, error))?;
  let expected_kind_matches = match &entry.kind {
    OutputKind::Directory => metadata.is_dir() && !metadata.file_type().is_symlink(),
    OutputKind::File { .. } => metadata.is_file() && !metadata.file_type().is_symlink(),
    OutputKind::Symlink { target, directory } => {
      if !metadata.file_type().is_symlink() {
        false
      } else {
        let current_target =
          fs::read_link(&entry.source).map_err(|error| io_error("reinspect output symlink", &entry.source, error))?;
        let current_directory = fs::metadata(&entry.source)
          .map_err(|error| io_error("reinspect output symlink target", &entry.source, error))?
          .is_dir();
        current_target.to_str() == Some(target.as_str()) && current_directory == *directory
      }
    },
  };
  if !expected_kind_matches || EntryKey::new(&entry.source, &metadata)? != entry.discovered {
    return Err(CacheError::UnstableFile {
      path: entry.source.clone(),
    });
  }
  Ok(())
}

fn copy_stable_file<W: Write>(
  entry: &OutputEntry,
  expected_length: u64,
  buffer: &mut [u8],
  writer: &mut W,
  cancel: &CancellationToken,
) -> CacheResult<()> {
  let before = fs::symlink_metadata(&entry.source)
    .map_err(|error| io_error("inspect output before bundling", &entry.source, error))?;
  if !before.is_file() || before.len() != expected_length {
    return Err(CacheError::UnstableFile {
      path: entry.source.clone(),
    });
  }
  let before_key = EntryKey::new(&entry.source, &before)?;
  if before_key != entry.discovered {
    return Err(CacheError::UnstableFile {
      path: entry.source.clone(),
    });
  }
  let mut file =
    File::open(&entry.source).map_err(|error| io_error("open output for bundling", &entry.source, error))?;
  let opened = file
    .metadata()
    .map_err(|error| io_error("inspect open output", &entry.source, error))?;
  if EntryKey::new(&entry.source, &opened)? != before_key {
    return Err(CacheError::UnstableFile {
      path: entry.source.clone(),
    });
  }
  let mut copied = 0_u64;
  loop {
    check_cancelled(cancel)?;
    let read = file
      .read(buffer)
      .map_err(|error| io_error("read output for bundling", &entry.source, error))?;
    if read == 0 {
      break;
    }
    writer.write_all(&buffer[..read]).map_err(CacheError::Stream)?;
    copied += read as u64;
  }
  let after = fs::symlink_metadata(&entry.source)
    .map_err(|error| io_error("inspect output after bundling", &entry.source, error))?;
  if copied != expected_length
    || !after.is_file()
    || EntryKey::new(&entry.source, &after)? != before_key
    || EntryKey::new(
      &entry.source,
      &file
        .metadata()
        .map_err(|error| io_error("reinspect open output", &entry.source, error))?,
    )? != before_key
  {
    return Err(CacheError::UnstableFile {
      path: entry.source.clone(),
    });
  }
  Ok(())
}

/// Forwards canonical bytes while enforcing expanded size and computing CAS identity.
struct CanonicalWriter<W> {
  inner: W,
  hasher: blake3::Hasher,
  bytes: u64,
  maximum: u64,
}

impl<W: Write> CanonicalWriter<W> {
  fn new(inner: W, maximum: u64) -> Self {
    Self {
      inner,
      hasher: blake3::Hasher::new(),
      bytes: 0,
      maximum,
    }
  }

  fn finish(self) -> CacheResult<(Digest, u64)> {
    Ok((
      Digest::new(DigestAlgorithm::Blake3, *self.hasher.finalize().as_bytes(), self.bytes),
      self.bytes,
    ))
  }
}

impl<W: Write> Write for CanonicalWriter<W> {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    let next = self
      .bytes
      .checked_add(buffer.len() as u64)
      .ok_or_else(|| io::Error::other("expanded bundle size overflowed"))?;
    if next > self.maximum {
      return Err(io::Error::other("expanded bundle exceeds configured limit"));
    }
    self.inner.write_all(buffer)?;
    self.hasher.update(buffer);
    self.bytes = next;
    Ok(buffer.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    self.inner.flush()
  }
}

/// Enforces the physical encoded-size limit without changing writer semantics.
struct CountingWriter<W> {
  inner: W,
  bytes: u64,
  maximum: u64,
}

impl<W> CountingWriter<W> {
  fn new(inner: W, maximum: u64) -> Self {
    Self {
      inner,
      bytes: 0,
      maximum,
    }
  }
}

impl<W: Write> Write for CountingWriter<W> {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    let remaining = self.maximum.saturating_sub(self.bytes);
    if buffer.len() as u64 > remaining {
      return Err(io::Error::other("encoded bundle exceeds configured limit"));
    }
    let written = self.inner.write(buffer)?;
    self.bytes += written as u64;
    Ok(written)
  }

  fn flush(&mut self) -> io::Result<()> {
    self.inner.flush()
  }
}

#[cfg(test)]
mod tests {
  use std::{cell::Cell, rc::Rc};

  use tempfile::TempDir;

  use super::*;

  /// Minimal writer used when a test must mutate the source during packing.
  struct CallbackWriter<F>(F);

  impl<F> Write for CallbackWriter<F>
  where
    F: FnMut(&[u8]) -> io::Result<()>,
  {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
      self.0(buffer)?;
      Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn stable_copy_rejects_wrong_length_and_mid_copy_mutation() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("file");
    fs::write(&source, vec![1_u8; 8192]).unwrap();
    let entry = OutputEntry {
      path: RelativePath::new("file").unwrap(),
      source: source.clone(),
      discovered: EntryKey::new(&source, &fs::symlink_metadata(&source).unwrap()).unwrap(),
      kind: OutputKind::File {
        length: 8192,
        executable: false,
      },
    };
    assert!(matches!(
      copy_stable_file(&entry, 7, &mut [0_u8; 4096], &mut Vec::new(), &CancellationToken::new()),
      Err(CacheError::UnstableFile { .. })
    ));
    fs::write(&source, vec![1_u8; 8193]).unwrap();
    assert!(matches!(
      copy_stable_file(
        &entry,
        8193,
        &mut [0_u8; 4096],
        &mut Vec::new(),
        &CancellationToken::new()
      ),
      Err(CacheError::UnstableFile { .. })
    ));
    fs::write(&source, vec![1_u8; 8192]).unwrap();
    let entry = OutputEntry {
      path: RelativePath::new("file").unwrap(),
      source: source.clone(),
      discovered: EntryKey::new(&source, &fs::symlink_metadata(&source).unwrap()).unwrap(),
      kind: OutputKind::File {
        length: 8192,
        executable: false,
      },
    };

    let mut changed = false;
    let mut writer = CallbackWriter(|_: &[u8]| {
      if !changed {
        fs::write(&source, vec![2_u8; 8193])?;
        changed = true;
      }
      Ok(())
    });
    assert!(matches!(
      copy_stable_file(&entry, 8192, &mut [0_u8; 4096], &mut writer, &CancellationToken::new()),
      Err(CacheError::UnstableFile { .. })
    ));
  }

  #[test]
  fn stable_tree_validation_rejects_directory_mutation() {
    let root = TempDir::new().unwrap();
    let output = root.path().join("out");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("first"), "first").unwrap();
    let metadata = fs::symlink_metadata(&output).unwrap();
    let entry = OutputEntry {
      path: RelativePath::new("out").unwrap(),
      source: output.clone(),
      discovered: EntryKey::new(&output, &metadata).unwrap(),
      kind: OutputKind::Directory,
    };

    fs::write(output.join("second"), "second").unwrap();
    assert!(matches!(
      validate_stable_entry(&entry),
      Err(CacheError::UnstableFile { .. })
    ));
  }

  #[test]
  fn traversal_enforces_one_budget_across_roots_and_directory_children() {
    let workspace = TempDir::new().unwrap();
    fs::write(workspace.path().join("first"), "first").unwrap();
    fs::write(workspace.path().join("second"), "second").unwrap();
    let limits = BundleLimits {
      max_entries: 1,
      ..BundleLimits::default()
    };
    assert!(matches!(
      visit_output_tree(
        workspace.path(),
        &[
          RelativePath::new("first").unwrap(),
          RelativePath::new("second").unwrap()
        ],
        limits,
        &CancellationToken::new(),
        &mut |_| Ok(())
      ),
      Err(CacheError::Limit(_))
    ));

    fs::create_dir(workspace.path().join("directory")).unwrap();
    fs::write(workspace.path().join("directory/child"), "child").unwrap();
    assert!(matches!(
      visit_output_tree(
        workspace.path(),
        &[RelativePath::new("directory").unwrap()],
        limits,
        &CancellationToken::new(),
        &mut |_| Ok(())
      ),
      Err(CacheError::Limit(_))
    ));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn directory_listing_rejects_non_utf8_names() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join(OsString::from_vec(vec![0xff])), "invalid").unwrap();
    assert!(matches!(
      read_sorted_children(directory.path(), 1),
      Err(CacheError::Path { .. })
    ));
  }

  #[test]
  fn traversal_rejects_a_directory_listing_changed_after_discovery() {
    let root = TempDir::new().unwrap();
    let output = root.path().join("out");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("first"), "first").unwrap();
    let mut changed = false;

    let result = visit_output_tree(
      root.path(),
      &[RelativePath::new("out").unwrap()],
      BundleLimits::default(),
      &CancellationToken::new(),
      &mut |entry| {
        // Mutate only after the original child list has been captured. The
        // directory's final listing must catch this even on filesystems whose
        // metadata timestamps are too coarse to expose the change reliably.
        if entry.path.as_str() == "out/first" && !changed {
          fs::write(output.join("second"), "second").unwrap();
          changed = true;
        }
        Ok(())
      },
    );

    assert!(matches!(result, Err(CacheError::UnstableFile { .. })));
  }

  #[test]
  fn final_tree_summary_rejects_mutation_of_an_already_written_file() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first");
    fs::write(&first, "first").unwrap();
    fs::write(root.path().join("second"), "second").unwrap();
    let mut changed = false;
    let result = pack_bundle(
      CallbackWriter(move |buffer: &[u8]| {
        if buffer == b"second" && !changed {
          fs::write(&first, "changed-length")?;
          changed = true;
        }
        Ok(())
      }),
      root.path(),
      &[
        RelativePath::new("first").unwrap(),
        RelativePath::new("second").unwrap(),
      ],
      BundleEncoding::Identity,
      BundleLimits::default(),
      &CancellationToken::new(),
    );

    assert!(matches!(result, Err(CacheError::UnstableFile { .. })));
  }

  #[cfg(unix)]
  #[test]
  fn stable_tree_validation_rejects_symlink_mutation() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    fs::write(root.path().join("first"), "first").unwrap();
    fs::write(root.path().join("other"), "other").unwrap();
    let link = root.path().join("link");
    symlink("first", &link).unwrap();
    let metadata = fs::symlink_metadata(&link).unwrap();
    let entry = OutputEntry {
      path: RelativePath::new("link").unwrap(),
      source: link.clone(),
      discovered: EntryKey::new(&link, &metadata).unwrap(),
      kind: OutputKind::Symlink {
        target: "first".to_owned(),
        directory: false,
      },
    };

    fs::remove_file(&link).unwrap();
    symlink("other", &link).unwrap();
    assert!(matches!(
      validate_stable_entry(&entry),
      Err(CacheError::UnstableFile { .. })
    ));

    fs::remove_file(&link).unwrap();
    fs::write(&link, "not a link").unwrap();
    assert!(matches!(
      validate_stable_entry(&entry),
      Err(CacheError::UnstableFile { .. })
    ));
  }

  #[test]
  fn counting_and_canonical_writers_enforce_limits_and_forward_flush() {
    #[derive(Clone)]
    struct FlushWriter(Rc<Cell<bool>>);
    impl Write for FlushWriter {
      fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Ok(buffer.len())
      }
      fn flush(&mut self) -> io::Result<()> {
        self.0.set(true);
        Ok(())
      }
    }

    let flushed = Rc::new(Cell::new(false));
    let mut canonical = CanonicalWriter::new(FlushWriter(flushed.clone()), 2);
    canonical.write_all(b"ok").unwrap();
    canonical.flush().unwrap();
    assert!(flushed.get());
    assert!(canonical.write_all(b"x").is_err());

    let mut counting = CountingWriter::new(Vec::new(), 2);
    counting.write_all(b"ok").unwrap();
    assert!(counting.write_all(b"x").is_err());
    counting.flush().unwrap();
  }
}
