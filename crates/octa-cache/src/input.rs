//! Portable input-tree discovery and content hashing.
//!
//! `InputSnapshotter` owns the semaphore shared by all of its clones. A runtime
//! should construct one instance and clone it for tasks, which prevents every
//! parallel task from creating an independent CPU-sized hashing pool.

use std::{
  collections::HashMap,
  fs::{self, Metadata},
  path::Path,
  sync::Arc,
};

use futures::{stream, StreamExt as _, TryStreamExt as _};
use octa_cache_protocol::{Digest, DigestAlgorithm, RelativePath};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
  error::{check_cancelled, io_error},
  fileset::collect,
  hash::{HashScheduler, HashedFile},
  platform::EntryKey,
  workspace::{portable_relative, safe_symlink_target},
  CacheError, CacheResult,
};

const INPUT_TREE_DOMAIN: &[u8] = b"octa.input-tree.v1";

pub use crate::hash::SnapshotOptions;

/// One canonical input-tree entry.
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputSnapshot {
  /// Domain-separated digest of the sorted canonical entries.
  pub root: Digest,
  /// Sorted entries retained for diagnostics and later action construction.
  pub entries: Vec<InputEntry>,
}

/// Bounded hasher shared by concurrently executing tasks.
#[derive(Clone)]
pub struct InputSnapshotter {
  scheduler: HashScheduler,
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
    })
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
    check_cancelled(cancel)?;
    let workspace =
      dunce::canonicalize(workspace).map_err(|error| io_error("canonicalize workspace", workspace, error))?;
    let discovery_root = workspace.clone();
    let discovery_patterns = patterns.to_vec();
    let discovery_cancel = cancel.clone();
    let max_entries = self.scheduler.options().max_entries;
    let paths = tokio::task::spawn_blocking(move || {
      collect(&discovery_patterns, &discovery_root, max_entries, &discovery_cancel)
    })
    .await
    .map_err(CacheError::Worker)??;

    // Completed hashes live only for this snapshot. This guarantees one read
    // per stable inode even when hardlinks fall outside the concurrent stream
    // window, without trusting metadata across later snapshots.
    let memo = Arc::new(Mutex::new(HashMap::<EntryKey, HashedFile>::new()));
    let concurrency = self.scheduler.options().max_parallel_hashes;
    let entries = stream::iter(paths.into_iter().map(|path| {
      let snapshotter = self.clone();
      let workspace = workspace.clone();
      let cancel = cancel.clone();
      let memo = memo.clone();
      async move { snapshotter.snapshot_path(&workspace, &path, &memo, &cancel).await }
    }))
    .buffer_unordered(concurrency)
    .try_collect::<Vec<_>>()
    .await?;

    let mut entries = entries;
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    Ok(InputSnapshot {
      root: input_root_digest(&entries),
      entries,
    })
  }

  async fn snapshot_path(
    &self,
    workspace: &Path,
    path: &Path,
    memo: &Mutex<HashMap<EntryKey, HashedFile>>,
    cancel: &CancellationToken,
  ) -> CacheResult<InputEntry> {
    check_cancelled(cancel)?;
    let relative = portable_relative(workspace, path)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error("inspect cache input", path, error))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
      let target = fs::read_link(path).map_err(|error| io_error("read cache input symlink", path, error))?;
      let target = safe_symlink_target(workspace, path, &target)?;
      return Ok(InputEntry::Symlink { path: relative, target });
    }
    if metadata.is_dir() {
      return Ok(InputEntry::Directory { path: relative });
    }
    if !metadata.is_file() {
      return Err(CacheError::UnsupportedEntry {
        path: path.to_path_buf(),
        kind: "only regular files, directories, and symlinks can be cache inputs",
      });
    }
    let hashed = self.hash_file(path, metadata, memo, cancel).await?;
    Ok(InputEntry::File {
      path: relative,
      content: hashed.content,
      executable: hashed.executable,
    })
  }

  async fn hash_file(
    &self,
    path: &Path,
    mut metadata: Metadata,
    memo: &Mutex<HashMap<EntryKey, HashedFile>>,
    cancel: &CancellationToken,
  ) -> CacheResult<HashedFile> {
    for _ in 0..=self.scheduler.options().mutation_retries {
      let key = EntryKey::new(path, &metadata)?;
      // Drop the async mutex guard before waiting for a hash; otherwise the
      // miss path would deadlock when it tried to publish the result below.
      let memoized = {
        let memo = memo.lock().await;
        memo.get(&key).cloned()
      };
      let hashed = match memoized {
        Some(hashed) => hashed,
        None => {
          let hashed = match self.scheduler.hash(path, key, cancel).await {
            Ok(hashed) => hashed,
            Err(CacheError::UnstableFile { .. }) => {
              metadata = fs::symlink_metadata(path).map_err(|error| io_error("reinspect cache input", path, error))?;
              continue;
            },
            Err(error) => return Err(error),
          };
          memo.lock().await.insert(hashed.key.clone(), hashed.clone());
          hashed
        },
      };
      // Each path is checked again even when its bytes came from a shared
      // hard-link read. Replacement of this particular directory entry must
      // invalidate the shared outcome.
      metadata = fs::symlink_metadata(path).map_err(|error| io_error("reinspect cache input", path, error))?;
      if metadata.is_file() && !metadata.file_type().is_symlink() && EntryKey::new(path, &metadata)? == hashed.key {
        return Ok(hashed);
      }
    }
    Err(CacheError::UnstableFile {
      path: path.to_path_buf(),
    })
  }
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
