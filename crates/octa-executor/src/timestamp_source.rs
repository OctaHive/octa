//! Modification-time source fingerprint strategy.
//!
//! Path identity is included alongside nanosecond timestamps so moving or
//! replacing a source invalidates freshness even when times collide.

use std::{path::PathBuf, time::UNIX_EPOCH};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::fs::symlink_metadata;
use tokio_util::sync::CancellationToken;

use crate::{
  error::{ExecutorError, ExecutorResult},
  path_hash,
  source_strategy::SourceStrategy,
};

pub struct TimestampSource;

#[async_trait]
impl SourceStrategy for TimestampSource {
  fn key(&self) -> &'static str {
    "timestamp"
  }

  fn compare_output_timestamps(&self) -> bool {
    true
  }

  async fn fingerprint(&self, sources: &[PathBuf], cancel_token: &CancellationToken) -> ExecutorResult<Vec<u8>> {
    check_cancelled(cancel_token)?;
    let mut hasher = Sha256::new();
    for path in sources {
      check_cancelled(cancel_token)?;
      path_hash::update_path_identity(&mut hasher, path);

      let modified = symlink_metadata(path).await?.modified()?;
      let elapsed = modified
        .duration_since(UNIX_EPOCH)
        .map_err(ExecutorError::CalculateDurationError)?;
      hasher.update(elapsed.as_secs().to_le_bytes());
      hasher.update(elapsed.subsec_nanos().to_le_bytes());
    }
    Ok(hasher.finalize().to_vec())
  }
}

fn check_cancelled(cancel_token: &CancellationToken) -> ExecutorResult<()> {
  if cancel_token.is_cancelled() {
    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "source fingerprint cancelled").into());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  async fn fingerprint(source: &TimestampSource, paths: &[PathBuf]) -> Vec<u8> {
    source.fingerprint(paths, &CancellationToken::new()).await.unwrap()
  }

  #[tokio::test]
  async fn empty_sources_have_a_stable_fingerprint() {
    let source = TimestampSource;

    assert_eq!(fingerprint(&source, &[]).await, fingerprint(&source, &[]).await);
  }

  #[tokio::test]
  async fn cancelled_fingerprint_stops_before_reading_sources() {
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = TimestampSource.fingerprint(&[], &cancel_token).await;

    assert!(matches!(
      result,
      Err(ExecutorError::IoError(error)) if error.kind() == std::io::ErrorKind::Interrupted
    ));
  }

  #[tokio::test]
  async fn modification_time_changes_the_fingerprint() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("source.txt");
    std::fs::write(&path, "initial").unwrap();
    let source = TimestampSource;
    let initial = fingerprint(&source, std::slice::from_ref(&path)).await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
    std::fs::write(&path, "changed").unwrap();

    assert_ne!(initial, fingerprint(&source, &[path]).await);
  }

  #[tokio::test]
  async fn nested_directory_changes_affect_the_fingerprint() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let file = nested.join("source.txt");
    std::fs::write(&file, "initial").unwrap();
    let source = TimestampSource;
    let paths = crate::source::collect(
      &[root.path().to_string_lossy().into_owned()],
      root.path(),
      &CancellationToken::new(),
    )
    .unwrap();
    let initial = fingerprint(&source, &paths).await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
    std::fs::write(file, "changed").unwrap();
    let paths = crate::source::collect(
      &[root.path().to_string_lossy().into_owned()],
      root.path(),
      &CancellationToken::new(),
    )
    .unwrap();

    assert_ne!(initial, fingerprint(&source, &paths).await);
  }
}
