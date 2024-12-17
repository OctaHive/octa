use std::{path::PathBuf, sync::Arc, time::UNIX_EPOCH};

use crate::{
  error::{ExecutorError, ExecutorResult},
  task::SourceStrategy,
};
use async_trait::async_trait;
use byteorder::{ByteOrder, LittleEndian};
use glob::glob;
use sled::Db;
use tokio::fs::metadata;

const TIMESTAMP_PREFIX: &str = "hash";

pub struct TimestampSource {
  fingerprint: Arc<Db>,
}

impl TimestampSource {
  pub fn new(fingerprint: Arc<Db>) -> Self {
    Self { fingerprint }
  }

  async fn get_file_modify_time(&self, path: PathBuf) -> ExecutorResult<u64> {
    match metadata(path).await {
      Ok(metadata) => {
        if let Ok(modified) = metadata.modified() {
          match modified.duration_since(UNIX_EPOCH) {
            Ok(duration) => Ok(duration.as_secs()),
            Err(e) => Err(ExecutorError::CalculateDurationError(e)),
          }
        } else {
          Ok(0)
        }
      },
      Err(e) => Err(ExecutorError::IoError(e)),
    }
  }
}

#[async_trait]
impl SourceStrategy for TimestampSource {
  async fn is_changed(&self, sources: Vec<String>) -> ExecutorResult<bool> {
    let mut has_changes = false;

    for source in sources {
      for entry in glob(&source)? {
        match entry {
          Ok(path) => {
            let new_timestamp = self.get_file_modify_time(path.clone()).await?;
            let path_str = path.display().to_string();

            let key = format!("{}_{}", TIMESTAMP_PREFIX, path_str.clone());
            if let Some(old_timestamp) = self.fingerprint.get(key.clone())? {
              let old_timestamp_str = LittleEndian::read_u64(&old_timestamp);
              if old_timestamp_str != new_timestamp {
                has_changes = true;
              }
            } else {
              has_changes = true;
            }

            let mut buf = [0u8; 8];
            LittleEndian::write_u64(&mut buf, new_timestamp);
            self.fingerprint.insert(key, &buf)?;
          },
          Err(e) => return Err(ExecutorError::GlobError(e)),
        }
      }
    }

    Ok(has_changes)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::SystemTime;
  use tempfile::{NamedTempFile, TempDir};

  #[tokio::test]
  async fn test_glob_error() {
    let db = sled::Config::new()
      .temporary(true)
      .open()
      .expect("Failed to open in-memory Sled database");

    let timestamp_source = TimestampSource::new(Arc::new(db));

    assert!(!timestamp_source
      .is_changed(vec!["SOME_MISSING_FILE".to_owned()])
      .await
      .is_err());
  }

  #[tokio::test]
  async fn test_get_file_modify_time() {
    let db = Arc::new(
      sled::Config::new()
        .temporary(true)
        .open()
        .expect("Failed to open in-memory Sled database"),
    );

    let temp_file = NamedTempFile::new().unwrap();

    // First, make sure the file doesn't exist before checking the mod time
    assert!(TimestampSource::new(Arc::clone(&db))
      .get_file_modify_time(temp_file.path().to_path_buf())
      .await
      .is_ok());

    let original_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    // Wait before change
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    std::fs::write(&temp_file, "test content").unwrap();

    assert!(TimestampSource::new(Arc::clone(&db))
      .get_file_modify_time(temp_file.path().to_path_buf())
      .await
      .is_ok());

    let new_timestamp = TimestampSource::new(Arc::clone(&db))
      .get_file_modify_time(temp_file.path().to_path_buf())
      .await
      .unwrap();

    assert!(original_timestamp < new_timestamp);
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
    let timestamp_source = TimestampSource::new(db);

    std::fs::write(&temp_file_path, "initial content").unwrap();

    assert!(timestamp_source
      .is_changed(vec![temp_file_path.display().to_string()])
      .await
      .is_ok());
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
    let timestamp_source = TimestampSource::new(db);

    std::fs::write(&temp_file_path, "initial content").unwrap();

    let is_changed = timestamp_source
      .is_changed(vec![temp_file_path.display().to_string()])
      .await
      .unwrap();

    assert!(is_changed);

    let is_changed = timestamp_source
      .is_changed(vec![temp_file_path.display().to_string()])
      .await
      .unwrap();

    assert!(is_changed == false);

    // Wait before change
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Modify the test file and check if it is detected as a change
    std::fs::write(&temp_file_path, "modified content").unwrap();

    let is_changed = timestamp_source
      .is_changed(vec![temp_file_path.display().to_string()])
      .await
      .unwrap();
    assert!(is_changed);
  }
}
