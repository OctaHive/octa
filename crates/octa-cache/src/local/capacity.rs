//! Durable cross-process byte accounting for the local cache layout.
//!
//! A fixed-width counter avoids scanning a large CAS every time a short-lived
//! runner starts. Writers reserve bytes before creating temporary data while an
//! advisory lock serializes updates across processes. Reservations are
//! deliberately pessimistic: a process crash can over-count, never under-count;
//! the next garbage-collection pass reconciles the counter with the filesystem.

use std::{fs, io::Read as _, io::Write as _, path::PathBuf};

use fs2::FileExt as _;

use super::{gc, object::open_lock_file};
use crate::{
  error::io_error,
  platform::{is_current_file, is_link_or_reparse},
  CacheError, CacheResult,
};

const STATE_FILE: &str = "capacity-v1";
const STATE_BYTES: u64 = size_of::<u64>() as u64;

#[derive(Clone, Debug)]
pub(super) struct CapacityLedger {
  state: PathBuf,
  lock: PathBuf,
}

impl CapacityLedger {
  pub(super) fn open(layout: PathBuf, max_entries: usize) -> CacheResult<Self> {
    let ledger = Self {
      state: layout.join(STATE_FILE),
      lock: layout.join("locks/capacity.lock"),
    };
    let lock = ledger.lock_sync()?;
    if ledger.read_sync().is_err() {
      let old_state_bytes = match fs::symlink_metadata(&ledger.state) {
        Ok(metadata) if metadata.is_file() && !is_link_or_reparse(&metadata) => metadata.len(),
        Ok(metadata) if is_link_or_reparse(&metadata) => {
          fs::remove_file(&ledger.state)
            .map_err(|error| io_error("remove unsafe local cache capacity state", &ledger.state, error))?;
          0
        },
        Ok(_) => {
          return Err(CacheError::Configuration(format!(
            "local cache capacity state '{}' must be a regular non-link file",
            ledger.state.display()
          )))
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(io_error("inspect local cache capacity state", &ledger.state, error)),
      };
      let measured = gc::directory_bytes(&layout, max_entries)?;
      let corrected = measured.saturating_sub(old_state_bytes).saturating_add(STATE_BYTES);
      ledger.write_sync(corrected)?;
    }
    drop(lock);
    Ok(ledger)
  }

  pub(super) async fn used(&self) -> CacheResult<u64> {
    let ledger = self.clone();
    run_blocking(move || {
      let _lock = ledger.lock_sync()?;
      ledger.read_sync()
    })
    .await
  }

  /// Atomically reserves `bytes` when the resulting usage fits below `limit`.
  pub(super) async fn try_reserve(&self, bytes: u64, limit: u64) -> CacheResult<bool> {
    let ledger = self.clone();
    run_blocking(move || {
      let _lock = ledger.lock_sync()?;
      let used = ledger.read_sync()?;
      let Some(reserved) = used.checked_add(bytes) else {
        return Err(CacheError::Limit("local cache byte accounting overflowed".to_owned()));
      };
      if reserved > limit {
        return Ok(false);
      }
      ledger.write_sync(reserved)?;
      Ok(true)
    })
    .await
  }

  /// Releases a reservation that did not result in a newly stored object.
  pub(super) async fn release(&self, bytes: u64) -> CacheResult<()> {
    let ledger = self.clone();
    run_blocking(move || {
      let _lock = ledger.lock_sync()?;
      let used = ledger.read_sync()?;
      ledger.write_sync(used.saturating_sub(bytes))
    })
    .await
  }

  /// Replaces pessimistic reservations with an exact post-GC measurement.
  pub(super) fn reconcile(&self, used: u64) -> CacheResult<()> {
    let _lock = self.lock_sync()?;
    self.write_sync(used)
  }

  fn lock_sync(&self) -> CacheResult<fs::File> {
    let file = open_lock_file(&self.lock)?;
    file
      .lock_exclusive()
      .map_err(|error| io_error("lock local cache capacity", &self.lock, error))?;
    Ok(file)
  }

  fn read_sync(&self) -> CacheResult<u64> {
    let metadata = fs::symlink_metadata(&self.state)
      .map_err(|error| io_error("inspect local cache capacity", &self.state, error))?;
    if !metadata.is_file() || is_link_or_reparse(&metadata) || metadata.len() != STATE_BYTES {
      return Err(CacheError::Metadata(
        "local cache capacity state must be an eight-byte regular non-link file".to_owned(),
      ));
    }
    let mut file =
      fs::File::open(&self.state).map_err(|error| io_error("read local cache capacity", &self.state, error))?;
    let current = fs::symlink_metadata(&self.state)
      .map_err(|error| io_error("re-inspect local cache capacity", &self.state, error))?;
    let same = is_current_file(&self.state, &file)?;
    if !same || !current.is_file() || is_link_or_reparse(&current) {
      return Err(CacheError::Metadata(
        "local cache capacity state changed while it was opened".to_owned(),
      ));
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    file
      .read_exact(&mut bytes)
      .map_err(|error| io_error("read local cache capacity", &self.state, error))?;
    let mut trailing = [0_u8; 1];
    if file
      .read(&mut trailing)
      .map_err(|error| io_error("read local cache capacity", &self.state, error))?
      != 0
    {
      return Err(CacheError::Metadata(
        "local cache capacity state has trailing bytes".to_owned(),
      ));
    }
    Ok(u64::from_be_bytes(bytes))
  }

  fn write_sync(&self, used: u64) -> CacheResult<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    let (mut file, reused) = match fs::symlink_metadata(&self.state) {
      Ok(metadata) if metadata.is_file() && !is_link_or_reparse(&metadata) => (
        options
          .open(&self.state)
          .map_err(|error| io_error("write local cache capacity", &self.state, error))?,
        true,
      ),
      Ok(_) => {
        return Err(CacheError::Configuration(format!(
          "local cache capacity state '{}' must be a regular non-link file",
          self.state.display()
        )))
      },
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
        options
          .create_new(true)
          .open(&self.state)
          .map_err(|error| io_error("create local cache capacity", &self.state, error))?,
        false,
      ),
      Err(error) => return Err(io_error("inspect local cache capacity", &self.state, error)),
    };
    if reused {
      let current = fs::symlink_metadata(&self.state)
        .map_err(|error| io_error("re-inspect local cache capacity", &self.state, error))?;
      let same = is_current_file(&self.state, &file)?;
      if !same || !current.is_file() || is_link_or_reparse(&current) {
        return Err(CacheError::Configuration(
          "local cache capacity state changed while it was opened".to_owned(),
        ));
      }
      file
        .set_len(0)
        .map_err(|error| io_error("truncate local cache capacity", &self.state, error))?;
    }
    file
      .write_all(&used.to_be_bytes())
      .and_then(|()| file.sync_all())
      .map_err(|error| io_error("write local cache capacity", &self.state, error))
  }
}

async fn run_blocking<T: Send + 'static>(
  operation: impl FnOnce() -> CacheResult<T> + Send + 'static,
) -> CacheResult<T> {
  tokio::task::spawn_blocking(operation)
    .await
    .map_err(CacheError::Worker)?
}
