//! Portable input-tree discovery and content hashing.
//!
//! `InputSnapshotter` owns the bounded hashing pool shared by all of its
//! clones. A runtime should construct one instance and clone it for tasks,
//! which prevents every parallel task from creating an independent CPU-sized
//! pool. An optional local CAS memo avoids rereading unchanged content without
//! changing the portable BLAKE3 action identity.

use std::{
  collections::HashMap,
  fmt,
  fs::{self, Metadata},
  path::{Path, PathBuf},
  sync::Arc,
};

use octa_cache_protocol::{Digest, DigestAlgorithm, RelativePath};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
  digest_memo::DigestMemo,
  error::{check_cancelled, io_error},
  fileset::collect_sets,
  hash::HashScheduler,
  platform::{persistent_memo_scope, EntryKey},
  workspace::{portable_relative, safe_symlink_target},
  CacheError, CacheResult, LocalCacheStore,
};

const INPUT_TREE_DOMAIN: &[u8] = b"octa.input-tree.v1";
const INPUT_METADATA_DOMAIN: &[u8] = b"octa.input-metadata.v1";

pub use crate::hash::SnapshotOptions;

/// One canonical input-tree entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputEntry {
  /// An empty or non-empty directory.
  Directory {
    /// Portable path relative to the workspace.
    path: RelativePath,
  },
  /// A regular file identified by content rather than metadata timestamps.
  File {
    /// Portable path relative to the workspace.
    path: RelativePath,
    /// Digest and byte length of the file content.
    content: Digest,
    /// Portable executable bit; always false on Windows.
    executable: bool,
  },
  /// A symbolic link whose relative target stays inside the workspace.
  Symlink {
    /// Portable path relative to the workspace.
    path: RelativePath,
    /// Original portable target text, preserved as part of the identity.
    target: String,
  },
}

impl InputEntry {
  fn path(&self) -> &RelativePath {
    match self {
      Self::Directory { path } | Self::File { path, .. } | Self::Symlink { path, .. } => path,
    }
  }
}

/// Complete sorted input tree and its canonical digest.
#[derive(Clone)]
pub struct InputSnapshot {
  /// Domain-separated digest of the sorted canonical entries.
  pub root: Digest,
  /// Sorted entries retained for diagnostics and later action construction.
  pub entries: Vec<InputEntry>,
  // Transient metadata is deliberately excluded from the portable input root.
  // It is valid only for detecting a change between lookup and restoration in
  // this process and is never serialized or trusted across Octa invocations.
  validation: SnapshotValidation,
}

impl fmt::Debug for InputSnapshot {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("InputSnapshot")
      .field("root", &self.root)
      .field("entries", &self.entries)
      .finish()
  }
}

impl PartialEq for InputSnapshot {
  fn eq(&self, other: &Self) -> bool {
    self.root == other.root && self.entries == other.entries
  }
}

impl Eq for InputSnapshot {}

#[derive(Clone)]
struct SnapshotValidation {
  workspace: PathBuf,
  entries: Vec<ValidationEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidationEntry {
  path: RelativePath,
  kind: ValidationKind,
  metadata: EntryKey,
}

/// One discovered regular file awaiting content hashing.
///
/// The validation index avoids retaining a second copy of its relative path and
/// lets a successful mutation retry replace the discovery metadata in place.
struct FileInput {
  validation_index: usize,
  path: PathBuf,
  key: EntryKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ValidationKind {
  Directory,
  File { executable: bool },
  Symlink(String),
}

/// Bounded hasher shared by concurrently executing tasks.
#[derive(Clone)]
pub struct InputSnapshotter {
  scheduler: HashScheduler,
  memo: Option<DigestMemo>,
}

impl InputSnapshotter {
  /// Creates a snapshotter whose concurrency budget is shared by all clones.
  ///
  /// # Errors
  ///
  /// Returns [`CacheError::Configuration`] when a concurrency or buffer bound
  /// is invalid.
  pub fn new(options: SnapshotOptions) -> CacheResult<Self> {
    Ok(Self {
      scheduler: HashScheduler::new(options)?,
      memo: None,
    })
  }

  /// Enables the optional digest memo backed by the already opened local CAS.
  ///
  /// Memo records never replace BLAKE3 in action identities. They only reuse a
  /// prior BLAKE3 digest while the platform file identity, size, modification
  /// time, and change time are unchanged. An unavailable or damaged memo is
  /// treated as an empty optimization record.
  ///
  pub fn with_digest_memo(mut self, store: Arc<LocalCacheStore>) -> Self {
    self.memo = Some(DigestMemo::new(store));
    self
  }

  /// Discovers and hashes a workspace-relative ordered input pattern set.
  ///
  /// # Errors
  ///
  /// Returns an error when the workspace or a selected entry is unsafe,
  /// unsupported, unstable, inaccessible, or when cancellation is requested.
  pub async fn snapshot(
    &self,
    workspace: &Path,
    patterns: &[String],
    cancel: &CancellationToken,
  ) -> CacheResult<InputSnapshot> {
    self
      .snapshot_pattern_sets(workspace, &[patterns.to_vec()], cancel)
      .await
  }

  /// Hashes the union of independent ordered input pattern sets.
  ///
  /// Exclusions apply only within their own set. This lets a user and several
  /// plugins add requirements without one contract removing another's files.
  pub async fn snapshot_pattern_sets(
    &self,
    workspace: &Path,
    pattern_sets: &[Vec<String>],
    cancel: &CancellationToken,
  ) -> CacheResult<InputSnapshot> {
    check_cancelled(cancel)?;
    let workspace =
      dunce::canonicalize(workspace).map_err(|error| io_error("canonicalize workspace", workspace, error))?;
    let discovery_root = workspace.clone();
    let discovery_patterns = pattern_sets.to_vec();
    let discovery_cancel = cancel.clone();
    let max_entries = self.scheduler.options().max_entries;
    let paths = tokio::task::spawn_blocking(move || {
      collect_sets(&discovery_patterns, &discovery_root, max_entries, &discovery_cancel)
    })
    .await
    .map_err(CacheError::Worker)??;
    let mut entries = Vec::with_capacity(paths.len());
    let mut validation_entries = Vec::with_capacity(paths.len());
    let mut files = Vec::new();
    for discovered in paths {
      check_cancelled(cancel)?;
      let validation = validation_entry(&workspace, &discovered.path, discovered.metadata)?;
      match &validation.kind {
        ValidationKind::File { .. } => files.push(FileInput {
          validation_index: validation_entries.len(),
          path: discovered.path,
          key: validation.metadata.clone(),
        }),
        ValidationKind::Directory => entries.push(InputEntry::Directory {
          path: validation.path.clone(),
        }),
        ValidationKind::Symlink(target) => entries.push(InputEntry::Symlink {
          path: validation.path.clone(),
          target: target.clone(),
        }),
      }
      validation_entries.push(validation);
    }

    // Cross-process reuse is enabled only when the platform can bind metadata
    // to a vetted local filesystem and the current OS boot. Unsupported and
    // remote filesystems retain the same content-hashing path without a memo.
    let memo_scope = self.memo.as_ref().and_then(|_| persistent_memo_scope(&workspace));
    let metadata = input_metadata_digest(&validation_entries, memo_scope.as_deref());
    let _memo_guard = match (&self.memo, metadata) {
      (Some(memo), Some(metadata)) => Some(tokio::select! {
        () = cancel.cancelled() => return Err(CacheError::Cancelled),
        guard = memo.lock(metadata) => guard,
      }),
      _ => None,
    };
    if let (Some(memo), Some(metadata)) = (&self.memo, metadata) {
      let lookup = tokio::select! {
        () = cancel.cancelled() => return Err(CacheError::Cancelled),
        lookup = memo.get(metadata, max_entries, self.scheduler.options().max_memo_bytes) => lookup,
      };
      match lookup {
        Ok(Some(snapshot)) if memo_matches(&snapshot, &validation_entries) => {
          return Ok(InputSnapshot {
            root: snapshot.root,
            entries: snapshot.entries,
            validation: SnapshotValidation {
              workspace,
              entries: validation_entries,
            },
          });
        },
        Ok(_) => {},
        Err(error) => {
          tracing::warn!(error = %error, "local input digest memo is unavailable; hashing inputs normally");
        },
      }
    }

    // Equal EntryKeys denote the same stable inode. Hash each key once inside
    // the batch, then reuse the result for hardlink aliases. This keeps the
    // small-file path allocation-light without restoring the former per-file
    // async task/channel graph.
    let (requests, request_indices, request_counts) = unique_hash_requests(&files);
    let hashes = self
      .scheduler
      .hash_many(requests, cancel)
      .await
      .into_iter()
      .collect::<CacheResult<Vec<_>>>()?;
    for (file, request_index) in files.into_iter().zip(request_indices) {
      let mut hashed = hashes[request_index].clone();
      // Every alias in a hardlink group is a separate directory entry. Confirm
      // that it still names the shared inode before assigning the shared digest;
      // a replacement gets its own bounded retry path.
      if request_counts[request_index] > 1 {
        let current = fs::symlink_metadata(&file.path)
          .map_err(|error| io_error("reinspect hardlink cache input", &file.path, error))?;
        if EntryKey::new(&file.path, &current)? != hashed.key {
          hashed = self.scheduler.hash(&file.path, file.key, cancel).await?;
        }
      }
      validation_entries[file.validation_index].metadata = hashed.key;
      entries.push(InputEntry::File {
        path: validation_entries[file.validation_index].path.clone(),
        content: hashed.content,
        executable: hashed.executable,
      });
    }

    entries.sort_by(|left, right| left.path().cmp(right.path()));
    let root = input_root_digest(&entries);
    if let (Some(memo), Some(metadata)) = (&self.memo, metadata) {
      let publication = tokio::select! {
        () = cancel.cancelled() => return Err(CacheError::Cancelled),
        publication = memo.put(metadata, root, &entries, self.scheduler.options().max_memo_bytes) => publication,
      };
      if let Err(error) = publication {
        tracing::warn!(error = %error, "failed to publish local input digest memo");
      }
    }
    Ok(InputSnapshot {
      root,
      entries,
      validation: SnapshotValidation {
        workspace,
        entries: validation_entries,
      },
    })
  }

  /// Checks that a previously hashed input snapshot is still current.
  ///
  /// This operation repeats pattern discovery and compares stable filesystem
  /// identity, size, modification/change times, entry kind, and symlink target.
  /// It never substitutes metadata for the BLAKE3 digest used in an action key:
  /// the optimization is valid only for the short interval between cache lookup
  /// and output restoration in one process. A mismatch returns `false` and the
  /// caller must not restore or publish under the earlier action identity.
  ///
  /// # Errors
  ///
  /// Returns an error when discovery is unsafe, inaccessible, unsupported, or
  /// cancelled. A different workspace returns `false` rather than reusing
  /// process-local metadata captured elsewhere.
  pub async fn revalidate(
    &self,
    workspace: &Path,
    pattern_sets: &[Vec<String>],
    snapshot: &InputSnapshot,
    cancel: &CancellationToken,
  ) -> CacheResult<bool> {
    check_cancelled(cancel)?;
    let workspace =
      dunce::canonicalize(workspace).map_err(|error| io_error("canonicalize workspace", workspace, error))?;
    if workspace != snapshot.validation.workspace {
      return Ok(false);
    }

    let discovery_root = workspace.clone();
    let discovery_patterns = pattern_sets.to_vec();
    let discovery_cancel = cancel.clone();
    let max_entries = self.scheduler.options().max_entries;
    let paths = tokio::task::spawn_blocking(move || {
      collect_sets(&discovery_patterns, &discovery_root, max_entries, &discovery_cancel)
    })
    .await
    .map_err(CacheError::Worker)??;
    if paths.len() != snapshot.validation.entries.len() {
      return Ok(false);
    }

    for (discovered, expected) in paths.into_iter().zip(&snapshot.validation.entries) {
      check_cancelled(cancel)?;
      if validation_entry(&workspace, &discovered.path, discovered.metadata)? != *expected {
        return Ok(false);
      }
    }
    Ok(true)
  }
}

fn validation_entry(workspace: &Path, path: &Path, metadata: Metadata) -> CacheResult<ValidationEntry> {
  let absolute = path;
  let path = portable_relative(workspace, absolute)?;
  let kind = if metadata.file_type().is_symlink() {
    let target = fs::read_link(absolute).map_err(|error| io_error("reread cache input symlink", absolute, error))?;
    ValidationKind::Symlink(safe_symlink_target(workspace, absolute, &target)?)
  } else if metadata.is_dir() {
    ValidationKind::Directory
  } else if metadata.is_file() {
    ValidationKind::File {
      executable: crate::platform::executable(&metadata),
    }
  } else {
    return Err(CacheError::UnsupportedEntry {
      path: absolute.to_path_buf(),
      kind: "only regular files, directories, and symlinks can be cache inputs",
    });
  };
  Ok(ValidationEntry {
    path,
    kind,
    metadata: EntryKey::new(absolute, &metadata)?,
  })
}

/// Collapses hardlink aliases to one content read while retaining a result
/// index and alias count for every original path.
fn unique_hash_requests(files: &[FileInput]) -> (Vec<(PathBuf, EntryKey)>, Vec<usize>, Vec<usize>) {
  let mut request_by_key = HashMap::<EntryKey, usize>::new();
  let mut requests = Vec::new();
  let mut indices = Vec::with_capacity(files.len());
  let mut counts = Vec::<usize>::new();
  for file in files {
    let index = match request_by_key.get(&file.key) {
      Some(index) => *index,
      None => {
        let index = requests.len();
        requests.push((file.path.clone(), file.key.clone()));
        request_by_key.insert(file.key.clone(), index);
        counts.push(0);
        index
      },
    };
    counts[index] += 1;
    indices.push(index);
  }
  (requests, indices, counts)
}

fn input_metadata_digest(entries: &[ValidationEntry], scope: Option<&[u8]>) -> Option<Digest> {
  let scope = scope?;
  let mut hasher = blake3::Hasher::new();
  let mut bytes = 0_u64;
  update_metadata(&mut hasher, &mut bytes, INPUT_METADATA_DOMAIN);
  update_metadata(&mut hasher, &mut bytes, scope);
  update_metadata(&mut hasher, &mut bytes, &(entries.len() as u64).to_be_bytes());
  for entry in entries {
    update_metadata(&mut hasher, &mut bytes, entry.path.as_str().as_bytes());
    match &entry.kind {
      ValidationKind::Directory => update_metadata(&mut hasher, &mut bytes, &[1]),
      ValidationKind::File { executable } => update_metadata(&mut hasher, &mut bytes, &[2, u8::from(*executable)]),
      ValidationKind::Symlink(target) => {
        update_metadata(&mut hasher, &mut bytes, &[3]);
        update_metadata(&mut hasher, &mut bytes, target.as_bytes());
      },
    }
    // Persistent file identities are fixed-field records, so they can feed the
    // aggregate directly. A per-entry digest would add 100,000 BLAKE3
    // initializations to the largest supported source tree without improving
    // collision resistance or canonical framing.
    bytes = bytes.checked_add(entry.metadata.update_memo_fingerprint(&mut hasher)?)?;
  }
  Some(Digest::new(
    DigestAlgorithm::Blake3,
    *hasher.finalize().as_bytes(),
    bytes,
  ))
}

fn update_metadata(hasher: &mut blake3::Hasher, bytes: &mut u64, value: &[u8]) {
  hasher.update(&(value.len() as u64).to_be_bytes());
  hasher.update(value);
  *bytes = bytes.saturating_add(8).saturating_add(value.len() as u64);
}

fn memo_matches(snapshot: &crate::digest_memo::MemoSnapshot, validation: &[ValidationEntry]) -> bool {
  snapshot.entries.len() == validation.len()
    && input_root_digest(&snapshot.entries) == snapshot.root
    && snapshot.entries.iter().zip(validation).all(|(entry, expected)| {
      entry.path() == &expected.path
        && match (entry, &expected.kind) {
          (InputEntry::Directory { .. }, ValidationKind::Directory) => true,
          (
            InputEntry::File {
              content, executable, ..
            },
            ValidationKind::File {
              executable: expected_executable,
            },
          ) => {
            content.algorithm() == DigestAlgorithm::Blake3
              && content.size_bytes() == expected.metadata.length()
              && executable == expected_executable
          },
          (InputEntry::Symlink { target, .. }, ValidationKind::Symlink(expected_target)) => target == expected_target,
          _ => false,
        }
    })
}

impl Default for InputSnapshotter {
  fn default() -> Self {
    Self::new(SnapshotOptions::default()).expect("default snapshot options are valid")
  }
}

/// Computes the persisted `octa.input-tree.v1` identity.
///
/// Entries have already been sorted by portable path. The stream begins with a
/// length-prefixed domain and entry count; every entry then contains a kind
/// tag, length-prefixed path, and kind-specific identity. File identity includes
/// executable state, digest algorithm, source byte length, and digest bytes;
/// symlink identity retains its target text. Timestamps and absolute paths are
/// deliberately absent. These bytes are part of the cache compatibility
/// contract documented in `docs/cache-formats-v1.md`; changing their order,
/// tags, or normalization requires a new domain/version.
fn input_root_digest(entries: &[InputEntry]) -> Digest {
  let mut hasher = blake3::Hasher::new();
  let mut bytes = 0_u64;
  let mut write = |value: &[u8]| {
    hasher.update(value);
    bytes += value.len() as u64;
  };
  write(&(INPUT_TREE_DOMAIN.len() as u32).to_be_bytes());
  write(INPUT_TREE_DOMAIN);
  write(&(entries.len() as u64).to_be_bytes());
  for entry in entries {
    let (tag, path) = match entry {
      InputEntry::Directory { path } => (1, path),
      InputEntry::File { path, .. } => (2, path),
      InputEntry::Symlink { path, .. } => (3, path),
    };
    write(&[tag]);
    write(&(path.as_str().len() as u32).to_be_bytes());
    write(path.as_str().as_bytes());
    match entry {
      InputEntry::Directory { .. } => {},
      InputEntry::File {
        content, executable, ..
      } => {
        let algorithm = match content.algorithm() {
          DigestAlgorithm::Blake3 => 1,
          DigestAlgorithm::Sha256 => 2,
        };
        write(&[u8::from(*executable), algorithm]);
        write(&content.size_bytes().to_be_bytes());
        write(&content.bytes());
      },
      InputEntry::Symlink { target, .. } => {
        write(&(target.len() as u32).to_be_bytes());
        write(target.as_bytes());
      },
    }
  }
  Digest::new(DigestAlgorithm::Blake3, *hasher.finalize().as_bytes(), bytes)
}

#[cfg(test)]
mod tests;
