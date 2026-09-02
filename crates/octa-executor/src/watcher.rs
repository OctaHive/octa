//! Polling-based source watcher used by the CLI watch mode.
//!
//! It builds a deterministic content snapshot from task source globs. Source
//! collection is shared with regular fingerprinting, so `.octaignore` rules
//! are applied consistently in both modes. Filesystem traversal and hashing
//! run on Tokio's blocking pool so large source trees do not stall async work.

use std::{io, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

use crate::{error::ExecutorResult, path_hash, source};

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
  targets: Arc<[WatchTarget]>,
  snapshot: SourceSnapshot,
  cancel_token: CancellationToken,
}

impl SourceWatcher {
  /// Captures the initial snapshot for the supplied targets.
  pub async fn new(targets: Vec<WatchTarget>, cancel_token: CancellationToken) -> ExecutorResult<Self> {
    let targets = Arc::<[WatchTarget]>::from(targets);
    let snapshot = capture(Arc::clone(&targets), cancel_token.clone()).await?;
    Ok(Self {
      targets,
      snapshot,
      cancel_token,
    })
  }

  /// Returns `true` when the resolved source set or file contents have changed.
  pub async fn poll(&mut self) -> ExecutorResult<bool> {
    let snapshot = match capture(Arc::clone(&self.targets), self.cancel_token.clone()).await {
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

async fn capture(targets: Arc<[WatchTarget]>, cancel_token: CancellationToken) -> ExecutorResult<SourceSnapshot> {
  tokio::task::spawn_blocking(move || SourceSnapshot::capture(targets.as_ref(), &cancel_token)).await?
}

impl SourceSnapshot {
  fn capture(targets: &[WatchTarget], cancel_token: &CancellationToken) -> ExecutorResult<Self> {
    let mut paths = Vec::new();
    for target in targets {
      paths.extend(source::collect(&target.sources, &target.root, cancel_token)?);
    }
    paths.sort_unstable();
    paths.dedup();

    Ok(Self(path_hash::content_fingerprint(&paths, cancel_token)?))
  }
}

#[cfg(test)]
mod tests {
  use std::{fs, path::Path};

  use tempfile::TempDir;

  use super::*;

  fn glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
  }

  #[tokio::test]
  async fn detects_created_modified_and_removed_sources() {
    let root = TempDir::new().unwrap();
    let source_dir = root.path().join("src");
    fs::create_dir(&source_dir).unwrap();
    let target = WatchTarget::new(vec![format!("{}/*", glob_path(&source_dir))], root.path().to_path_buf());
    let mut watcher = SourceWatcher::new(vec![target], CancellationToken::new())
      .await
      .unwrap();

    assert!(!watcher.poll().await.unwrap());

    let source = source_dir.join("main.rs");
    fs::write(&source, "first").unwrap();
    assert!(watcher.poll().await.unwrap());
    assert!(!watcher.poll().await.unwrap());

    fs::write(&source, "second").unwrap();
    assert!(watcher.poll().await.unwrap());

    fs::remove_file(source).unwrap();
    assert!(watcher.poll().await.unwrap());
  }

  #[tokio::test]
  async fn ignores_octaignore_matches() {
    let root = TempDir::new().unwrap();
    let source_dir = root.path().join("src");
    fs::create_dir(&source_dir).unwrap();
    fs::write(root.path().join(".octaignore"), "*.tmp\n").unwrap();
    let target = WatchTarget::new(vec![format!("{}/*", glob_path(&source_dir))], root.path().to_path_buf());
    let mut watcher = SourceWatcher::new(vec![target], CancellationToken::new())
      .await
      .unwrap();

    fs::write(source_dir.join("cache.tmp"), "ignored").unwrap();
    assert!(!watcher.poll().await.unwrap());

    fs::write(source_dir.join("main.rs"), "tracked").unwrap();
    assert!(watcher.poll().await.unwrap());
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn detects_retargeted_directory_symlinks_without_following_them() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let link = root.path().join("current");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    symlink(&first, &link).unwrap();
    let target = WatchTarget::new(vec![glob_path(&link)], root.path().to_path_buf());
    let mut watcher = SourceWatcher::new(vec![target], CancellationToken::new())
      .await
      .unwrap();

    fs::remove_file(&link).unwrap();
    symlink(&second, link).unwrap();

    assert!(watcher.poll().await.unwrap());
  }

  #[tokio::test]
  async fn cancellation_interrupts_initial_snapshot() {
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = SourceWatcher::new(Vec::new(), cancel_token).await;

    assert!(matches!(
      result,
      Err(crate::error::ExecutorError::IoError(error)) if error.kind() == io::ErrorKind::Interrupted
    ));
  }
}
