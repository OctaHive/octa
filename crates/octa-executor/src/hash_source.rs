use std::path::PathBuf;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{error::ExecutorResult, path_hash, source_strategy::SourceStrategy};

pub struct HashSource;

#[async_trait]
impl SourceStrategy for HashSource {
  fn key(&self) -> &'static str {
    "hash"
  }

  async fn fingerprint(&self, sources: &[PathBuf], cancel_token: &CancellationToken) -> ExecutorResult<Vec<u8>> {
    let sources = sources.to_vec();
    let cancel_token = cancel_token.clone();
    Ok(
      tokio::task::spawn_blocking(move || path_hash::content_fingerprint(&sources, &cancel_token))
        .await??
        .to_vec(),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  async fn fingerprint(source: &HashSource, paths: &[PathBuf]) -> Vec<u8> {
    source.fingerprint(paths, &CancellationToken::new()).await.unwrap()
  }

  #[tokio::test]
  async fn empty_sources_have_a_stable_fingerprint() {
    let source = HashSource;

    assert_eq!(fingerprint(&source, &[]).await, fingerprint(&source, &[]).await);
  }

  #[tokio::test]
  async fn cancelled_fingerprint_stops_before_reading_sources() {
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = HashSource.fingerprint(&[], &cancel_token).await;

    assert!(matches!(
      result,
      Err(crate::error::ExecutorError::IoError(error)) if error.kind() == std::io::ErrorKind::Interrupted
    ));
  }

  #[tokio::test]
  async fn content_and_path_changes_affect_the_fingerprint() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first.txt");
    let second = root.path().join("second.txt");
    std::fs::write(&first, "initial").unwrap();
    std::fs::write(&second, "initial").unwrap();
    let source = HashSource;

    let initial = fingerprint(&source, std::slice::from_ref(&first)).await;
    std::fs::write(&first, "changed").unwrap();
    let changed = fingerprint(&source, std::slice::from_ref(&first)).await;
    let other_path = fingerprint(&source, std::slice::from_ref(&second)).await;

    assert_ne!(initial, changed);
    assert_ne!(changed, other_path);
  }

  #[tokio::test]
  async fn nested_directory_changes_affect_the_fingerprint() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let file = nested.join("source.txt");
    std::fs::write(&file, "initial").unwrap();
    let source = HashSource;
    let paths = crate::source::collect(
      &[root.path().to_string_lossy().into_owned()],
      root.path(),
      &CancellationToken::new(),
    )
    .unwrap();
    let initial = fingerprint(&source, &paths).await;

    std::fs::write(file, "changed").unwrap();
    let paths = crate::source::collect(
      &[root.path().to_string_lossy().into_owned()],
      root.path(),
      &CancellationToken::new(),
    )
    .unwrap();

    assert_ne!(initial, fingerprint(&source, &paths).await);
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn retargeting_a_directory_symlink_changes_the_fingerprint() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let link = root.path().join("current");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    symlink(&first, &link).unwrap();
    let source = HashSource;
    let initial = fingerprint(&source, std::slice::from_ref(&link)).await;

    std::fs::remove_file(&link).unwrap();
    symlink(&second, &link).unwrap();

    assert_ne!(initial, fingerprint(&source, &[link]).await);
  }
}
