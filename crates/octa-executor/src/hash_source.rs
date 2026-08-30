use std::{fmt::Write, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sled::Db;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::{error::ExecutorResult, task::SourceStrategy};

pub struct HashSource {
  fingerprint: Arc<Db>,
}

const HASH_PREFIX: &str = "hash";

impl HashSource {
  pub fn new(fingerprint: Arc<Db>) -> Self {
    Self { fingerprint }
  }

  async fn calculate_file_hash(&self, path: PathBuf) -> ExecutorResult<String> {
    let mut file = File::open(path).await?;
    let mut buffer = [0u8; 1024];
    let mut hasher = Sha256::new();

    loop {
      let bytes_read = file.read(&mut buffer).await?;

      if bytes_read == 0 {
        break;
      }

      hasher.update(&buffer[..bytes_read]);
    }

    let digest = hasher.finalize();
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
      write!(&mut hash, "{byte:02x}").expect("writing to a String cannot fail");
    }

    Ok(hash)
  }
}

#[async_trait]
impl SourceStrategy for HashSource {
  async fn is_changed(&self, sources: Vec<PathBuf>) -> ExecutorResult<bool> {
    let mut has_changes = false;

    for path in sources {
      let new_hash = self.calculate_file_hash(path.clone()).await?;
      let path_str = path.display().to_string();

      let key = format!("{}_{}", HASH_PREFIX, path_str);
      if let Some(old_hash) = self.fingerprint.get(key.clone())? {
        let old_hash_str = String::from_utf8_lossy(&old_hash);

        if old_hash_str != new_hash {
          has_changes = true;
        }
      } else {
        has_changes = true;
      }

      self.fingerprint.insert(key, new_hash.as_bytes())?;
    }

    Ok(has_changes)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::{NamedTempFile, TempDir};

  #[tokio::test]
  async fn test_no_sources() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let hash_source = HashSource::new(Arc::new(db));

    assert!(!hash_source.is_changed(vec![]).await.unwrap());
  }

  #[tokio::test]
  async fn test_calculate_file_hash() {
    let db = Arc::new(
      sled::Config::new()
        .temporary(true)
        .open()
        .expect("Failed to open in-memory Sled database"),
    );

    let temp_file = NamedTempFile::new().unwrap();
    let hash_source = HashSource::new(Arc::clone(&db));

    // First, make sure the file doesn't exist before checking the hash
    assert!(hash_source
      .calculate_file_hash(temp_file.path().to_path_buf())
      .await
      .is_ok());

    let old_hash = hash_source
      .calculate_file_hash(temp_file.path().to_path_buf())
      .await
      .unwrap();

    std::fs::write(&temp_file, "test content").unwrap();

    assert!(hash_source
      .calculate_file_hash(temp_file.path().to_path_buf())
      .await
      .is_ok());

    let new_hash = hash_source
      .calculate_file_hash(temp_file.path().to_path_buf())
      .await
      .unwrap();

    assert!(old_hash != new_hash);
  }

  #[tokio::test]
  async fn test_changed_no_changes() {
    let db = Arc::new(
      sled::Config::new()
        .temporary(true)
        .open()
        .expect("Failed to open in-memory Sled database"),
    );

    let temp_dir = TempDir::new().unwrap();
    let temp_file_path = temp_dir.path().join("test_file");
    let timestamp_source = HashSource::new(db);

    std::fs::write(&temp_file_path, "initial content").unwrap();

    assert!(timestamp_source.is_changed(vec![temp_file_path.clone()]).await.is_ok());
  }

  #[tokio::test]
  async fn test_changed_file_changes() {
    let db = Arc::new(
      sled::Config::new()
        .temporary(true)
        .open()
        .expect("Failed to open in-memory Sled database"),
    );

    let temp_dir = TempDir::new().unwrap();
    let temp_file_path = temp_dir.path().join("test_file");
    let timestamp_source = HashSource::new(db);

    std::fs::write(&temp_file_path, "initial content").unwrap();

    let is_changed = timestamp_source.is_changed(vec![temp_file_path.clone()]).await.unwrap();

    assert!(is_changed);

    let is_changed = timestamp_source.is_changed(vec![temp_file_path.clone()]).await.unwrap();

    assert!(is_changed == false);

    // Modify the test file and check if it is detected as a change
    std::fs::write(&temp_file_path, "modified content").unwrap();

    let is_changed = timestamp_source.is_changed(vec![temp_file_path]).await.unwrap();
    assert!(is_changed);
  }
}
