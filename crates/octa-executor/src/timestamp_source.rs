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
            Err(e) => {
              return Err(ExecutorError::CalculateDurationError(e));
            },
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
  async fn changed(&self, sources: Vec<String>) -> ExecutorResult<bool> {
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
