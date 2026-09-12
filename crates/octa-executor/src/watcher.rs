//! Polling watch mode over the same canonical input snapshots as caching.
//!
//! A target contains only the task's portable input patterns and workspace.
//! [`SourceWatcher`] receives the runtime-owned [`InputSnapshotter`], so watch
//! and cache share selector semantics and the hashing concurrency budget.

use std::{path::PathBuf, sync::Arc};

use octa_cache::InputSnapshotter;
use octa_cache_protocol::Digest;
use tokio_util::sync::CancellationToken;

use crate::error::{ExecutorError, ExecutorResult};

/// Input set belonging to one task invocation.
#[derive(Clone, Debug)]
pub struct WatchTarget {
  /// Ordered include/exclude patterns from `files.inputs`.
  pub inputs: Vec<String>,
  /// Workspace that bounds discovery and hierarchical `.octaignore` files.
  pub workspace: PathBuf,
}

impl WatchTarget {
  pub fn new(inputs: Vec<String>, workspace: PathBuf) -> Self {
    Self { inputs, workspace }
  }
}

/// Digest sequence for every watched task input set.
#[derive(Debug, Eq, PartialEq)]
struct SourceSnapshot(Vec<Digest>);

/// Detects source changes by comparing canonical task input roots between polls.
pub struct SourceWatcher {
  targets: Arc<[WatchTarget]>,
  snapshotter: InputSnapshotter,
  snapshot: SourceSnapshot,
  cancel_token: CancellationToken,
}

impl SourceWatcher {
  /// Uses default bounds for standalone embedding callers.
  pub async fn new(targets: Vec<WatchTarget>, cancel_token: CancellationToken) -> ExecutorResult<Self> {
    Self::with_snapshotter(targets, InputSnapshotter::default(), cancel_token).await
  }

  /// Captures the initial state through a runtime-owned hashing service.
  pub async fn with_snapshotter(
    targets: Vec<WatchTarget>,
    snapshotter: InputSnapshotter,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<Self> {
    let targets = Arc::<[WatchTarget]>::from(targets);
    let snapshot = capture(&snapshotter, &targets, &cancel_token).await?;
    Ok(Self {
      targets,
      snapshotter,
      snapshot,
      cancel_token,
    })
  }

  /// Returns true when any task's selected path set or content changed.
  pub async fn poll(&mut self) -> ExecutorResult<bool> {
    let snapshot = capture(&self.snapshotter, &self.targets, &self.cancel_token).await?;
    if snapshot == self.snapshot {
      return Ok(false);
    }
    self.snapshot = snapshot;
    Ok(true)
  }
}

async fn capture(
  snapshotter: &InputSnapshotter,
  targets: &[WatchTarget],
  cancel_token: &CancellationToken,
) -> ExecutorResult<SourceSnapshot> {
  let mut roots = Vec::with_capacity(targets.len());
  for target in targets {
    roots.push(
      snapshotter
        .snapshot(&target.workspace, &target.inputs, cancel_token)
        .await
        .map_err(ExecutorError::from)?
        .root,
    );
  }
  Ok(SourceSnapshot(roots))
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  #[tokio::test]
  async fn detects_created_modified_removed_and_ignored_inputs() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join(".octaignore"), "*.tmp\n").unwrap();
    let target = WatchTarget::new(vec!["src/**/*".to_owned()], root.path().to_path_buf());
    let mut watcher = SourceWatcher::new(vec![target], CancellationToken::new())
      .await
      .unwrap();

    fs::write(root.path().join("src/cache.tmp"), "ignored").unwrap();
    assert!(!watcher.poll().await.unwrap());
    let source = root.path().join("src/main.rs");
    fs::write(&source, "first").unwrap();
    assert!(watcher.poll().await.unwrap());
    assert!(!watcher.poll().await.unwrap());
    fs::write(&source, "second").unwrap();
    assert!(watcher.poll().await.unwrap());
    fs::remove_file(source).unwrap();
    assert!(watcher.poll().await.unwrap());
  }

  #[tokio::test]
  async fn cancellation_interrupts_initial_snapshot() {
    let root = TempDir::new().unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let result = SourceWatcher::new(vec![WatchTarget::new(Vec::new(), root.path().to_path_buf())], token).await;
    assert!(matches!(result, Err(ExecutorError::Cache(error)) if error.to_string().contains("cancel")));
  }
}
