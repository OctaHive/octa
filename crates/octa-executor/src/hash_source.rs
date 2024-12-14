use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use glob::glob;
use sha2::{Digest, Sha256};
use sled::Db;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::{
  error::{ExecutorError, ExecutorResult},
  task::SourceStrategy,
};

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

    Ok(format!("{:x}", hasher.finalize()))
  }
}

#[async_trait]
impl SourceStrategy for HashSource {
  async fn is_changed(&self, sources: Vec<String>) -> ExecutorResult<bool> {
    let mut has_changes = false;

    for source in sources {
      for entry in glob(&source)? {
        match entry {
          Ok(path) => {
            let new_hash = self.calculate_file_hash(path.clone()).await?;
            let path_str = path.display().to_string();

            let key = format!("{}_{}", HASH_PREFIX, path_str.clone());
            if let Some(old_hash) = self.fingerprint.get(key.clone())? {
              let old_hash_str = String::from_utf8_lossy(&old_hash);

              if old_hash_str != new_hash {
                has_changes = true;
              }
            } else {
              has_changes = true;
            }

            self.fingerprint.insert(key, new_hash.as_bytes())?;
          },
          Err(e) => return Err(ExecutorError::GlobError(e)),
        }
      }
    }

    Ok(has_changes)
  }
}
