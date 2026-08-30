//! Polling-based source watcher used by the CLI watch mode.
//!
//! It builds a deterministic content snapshot from task source globs. Source
//! collection is shared with regular fingerprinting, so `.octaignore` rules
//! are applied consistently in both modes.

use std::{
  fs::File,
  io::{self, Read},
  path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{error::ExecutorResult, source};

/// Sources belonging to a task and the root used to resolve ignore rules.
#[derive(Clone, Debug)]
pub struct WatchTarget {
  /// Glob patterns configured in the task's `sources` field.
  pub sources: Vec<String>,

  /// Root Octafile directory that bounds `.octaignore` discovery.
  pub root: PathBuf,
}

impl WatchTarget {
  /// Creates a watch target from task source patterns and its Octafile root.
  pub fn new(sources: Vec<String>, root: PathBuf) -> Self {
    Self { sources, root }
  }
}

/// Digest of the complete set of resolved source paths and their contents.
#[derive(Debug, Eq, PartialEq)]
struct SourceSnapshot([u8; 32]);

/// Detects source changes by comparing snapshots captured between polls.
pub struct SourceWatcher {
  targets: Vec<WatchTarget>,
  snapshot: SourceSnapshot,
}

impl SourceWatcher {
  /// Captures the initial snapshot for the supplied targets.
  pub fn new(targets: Vec<WatchTarget>) -> ExecutorResult<Self> {
    let snapshot = SourceSnapshot::capture(&targets)?;
    Ok(Self { targets, snapshot })
  }

  /// Returns `true` when the resolved source set or file contents have changed.
  pub fn poll(&mut self) -> ExecutorResult<bool> {
    let snapshot = match SourceSnapshot::capture(&self.targets) {
      Ok(snapshot) => snapshot,
      // A source can disappear after glob expansion but before it is opened.
      // Keep the previous snapshot and retry on the next poll; the deletion
      // will then be represented by the updated set of resolved paths.
      Err(crate::error::ExecutorError::IoError(error)) if error.kind() == io::ErrorKind::NotFound => {
        return Ok(false);
      },
      Err(error) => return Err(error),
    };
    if snapshot == self.snapshot {
      return Ok(false);
    }

    self.snapshot = snapshot;
    Ok(true)
  }
}

impl SourceSnapshot {
  fn capture(targets: &[WatchTarget]) -> ExecutorResult<Self> {
    let mut paths = Vec::new();
    for target in targets {
      paths.extend(source::collect(&target.sources, &target.root)?);
    }
    paths.sort_unstable();
    paths.dedup();

    let mut hasher = Sha256::new();
    for path in paths {
      hash_path(&mut hasher, &path)?;
    }

    Ok(Self(hasher.finalize().into()))
  }
}

fn hash_path(hasher: &mut Sha256, path: &Path) -> ExecutorResult<()> {
  let path_text = path.to_string_lossy();
  // Length prefixes and entry type markers keep adjacent hash inputs from
  // producing ambiguous byte sequences.
  hasher.update((path_text.len() as u64).to_le_bytes());
  hasher.update(path_text.as_bytes());

  if path.is_dir() {
    hasher.update([0]);
    return Ok(());
  }

  hasher.update([1]);
  let mut file = File::open(path)?;
  hasher.update(file.metadata()?.len().to_le_bytes());
  let mut buffer = [0; 8192];
  loop {
    let read = file.read(&mut buffer)?;
    if read == 0 {
      break;
    }
    hasher.update(&buffer[..read]);
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  fn glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
  }

  #[test]
  fn detects_created_modified_and_removed_sources() {
    let root = TempDir::new().unwrap();
    let source_dir = root.path().join("src");
    fs::create_dir(&source_dir).unwrap();
    let target = WatchTarget::new(vec![format!("{}/*", glob_path(&source_dir))], root.path().to_path_buf());
    let mut watcher = SourceWatcher::new(vec![target]).unwrap();

    assert!(!watcher.poll().unwrap());

    let source = source_dir.join("main.rs");
    fs::write(&source, "first").unwrap();
    assert!(watcher.poll().unwrap());
    assert!(!watcher.poll().unwrap());

    fs::write(&source, "second").unwrap();
    assert!(watcher.poll().unwrap());

    fs::remove_file(source).unwrap();
    assert!(watcher.poll().unwrap());
  }

  #[test]
  fn ignores_octaignore_matches() {
    let root = TempDir::new().unwrap();
    let source_dir = root.path().join("src");
    fs::create_dir(&source_dir).unwrap();
    fs::write(root.path().join(".octaignore"), "*.tmp\n").unwrap();
    let target = WatchTarget::new(vec![format!("{}/*", glob_path(&source_dir))], root.path().to_path_buf());
    let mut watcher = SourceWatcher::new(vec![target]).unwrap();

    fs::write(source_dir.join("cache.tmp"), "ignored").unwrap();
    assert!(!watcher.poll().unwrap());

    fs::write(source_dir.join("main.rs"), "tracked").unwrap();
    assert!(watcher.poll().unwrap());
  }
}
