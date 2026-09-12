//! Bounded, cancellation-aware hashing of stable regular files.
//!
//! `HashScheduler` owns the process-local in-flight table and the semaphore
//! shared by every cloned snapshotter. An in-flight read has an explicit
//! waiter count: cancelling one caller does not affect the others, while the
//! last departing caller cancels the underlying chunked read. Completed jobs
//! normally leave the table immediately; a closed worker channel is replaced
//! on the next request. A snapshot keeps its own short-lived memo so sequential
//! hardlinks still reuse the digest without becoming a stale process-wide
//! metadata cache.

use std::{
  collections::HashMap,
  fs::{self, File},
  io::Read,
  path::{Path, PathBuf},
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  },
};

use octa_cache_protocol::{Digest, DigestAlgorithm};
use tokio::sync::{watch, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{
  error::io_error,
  platform::{executable, EntryKey},
  CacheError, CacheResult,
};

type SharedHashResult = Result<HashedFile, SharedHashError>;

/// Operational hashing settings centralized at the composition root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotOptions {
  /// Maximum number of files hashed concurrently across all cloned snapshotters.
  pub max_parallel_hashes: usize,
  /// Maximum filesystem entries visited by one input snapshot.
  pub max_entries: usize,
  /// Reused per-file streaming buffer size.
  pub read_buffer_bytes: usize,
  /// Additional attempts after a file changes during its first read.
  pub mutation_retries: u8,
}

impl Default for SnapshotOptions {
  fn default() -> Self {
    Self {
      max_parallel_hashes: std::thread::available_parallelism().map_or(1, usize::from),
      max_entries: 1_000_000,
      read_buffer_bytes: 1024 * 1024,
      mutation_retries: 2,
    }
  }
}

impl SnapshotOptions {
  fn validate(self) -> CacheResult<Self> {
    if self.max_parallel_hashes == 0 || self.max_entries == 0 {
      return Err(CacheError::Configuration(
        "max_parallel_hashes and max_entries must be greater than zero".to_owned(),
      ));
    }
    if !(4 * 1024..=16 * 1024 * 1024).contains(&self.read_buffer_bytes) {
      return Err(CacheError::Configuration(
        "read_buffer_bytes must be between 4 KiB and 16 MiB".to_owned(),
      ));
    }
    Ok(self)
  }
}

/// Runtime-wide hashing budget and coalescing table.
#[derive(Clone)]
pub(crate) struct HashScheduler {
  inner: Arc<SchedulerInner>,
}

/// State shared by every snapshotter clone in this process.
struct SchedulerInner {
  options: SnapshotOptions,
  permits: Arc<Semaphore>,
  in_flight: Mutex<HashMap<EntryKey, Arc<HashJob>>>,
}

/// One coalesced read keyed by a transient identity of the opened file.
struct HashJob {
  // The worker owns the only sender. Its unexpected disappearance therefore
  // closes this channel and wakes all waiters instead of hanging them.
  result: watch::Receiver<Option<SharedHashResult>>,
  cancel: CancellationToken,
  waiters: AtomicUsize,
}

/// Per-caller subscription whose drop participates in worker cancellation.
struct HashWaiter {
  job: Arc<HashJob>,
  result: watch::Receiver<Option<SharedHashResult>>,
}

impl HashScheduler {
  pub(crate) fn new(options: SnapshotOptions) -> CacheResult<Self> {
    let options = options.validate()?;
    Ok(Self {
      inner: Arc::new(SchedulerInner {
        options,
        permits: Arc::new(Semaphore::new(options.max_parallel_hashes)),
        in_flight: Mutex::new(HashMap::new()),
      }),
    })
  }

  pub(crate) fn options(&self) -> SnapshotOptions {
    self.inner.options
  }

  pub(crate) async fn hash(&self, path: &Path, key: EntryKey, cancel: &CancellationToken) -> CacheResult<HashedFile> {
    let (waiter, worker) = self.waiter(key.clone()).await;
    if let Some(result) = worker {
      self.start(key, path.to_path_buf(), waiter.job.clone(), result);
    }
    waiter.wait(cancel).await?.map_err(|error| error.into_cache_error(path))
  }

  async fn waiter(&self, key: EntryKey) -> (HashWaiter, Option<watch::Sender<Option<SharedHashResult>>>) {
    let mut in_flight = self.inner.in_flight.lock().await;
    let (job, worker) = match in_flight.get(&key) {
      // A closed result channel denotes an abandoned worker and must never be
      // reused, even if its cleanup task did not remove the table entry.
      Some(job) if !job.cancel.is_cancelled() && job.result.has_changed().is_ok() => (job.clone(), None),
      _ => {
        let (result, receiver) = watch::channel(None);
        let job = Arc::new(HashJob {
          result: receiver,
          cancel: CancellationToken::new(),
          waiters: AtomicUsize::new(0),
        });
        in_flight.insert(key, job.clone());
        (job, Some(result))
      },
    };
    (HashWaiter::new(job), worker)
  }

  fn start(&self, key: EntryKey, path: PathBuf, job: Arc<HashJob>, result: watch::Sender<Option<SharedHashResult>>) {
    let inner = self.inner.clone();
    tokio::spawn(async move {
      let outcome = run_hash_job(&inner, &path, &job).await;
      result.send_replace(Some(outcome));

      let mut in_flight = inner.in_flight.lock().await;
      if in_flight.get(&key).is_some_and(|current| Arc::ptr_eq(current, &job)) {
        in_flight.remove(&key);
      }
    });
  }

  #[cfg(test)]
  pub(crate) async fn hold_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
    self
      .inner
      .permits
      .clone()
      .acquire_owned()
      .await
      .expect("the private hashing semaphore is never closed")
  }
}

impl HashWaiter {
  fn new(job: Arc<HashJob>) -> Self {
    job.waiters.fetch_add(1, Ordering::Relaxed);
    let result = job.result.clone();
    Self { job, result }
  }

  async fn wait(mut self, cancel: &CancellationToken) -> CacheResult<SharedHashResult> {
    loop {
      if let Some(result) = self.result.borrow().clone() {
        return Ok(result);
      }
      tokio::select! {
        () = cancel.cancelled() => return Err(CacheError::Cancelled),
        changed = self.result.changed() => {
          if changed.is_err() {
            return Err(CacheError::WorkerTerminated(
              "hash worker ended without a result".to_owned(),
            ));
          }
        },
      }
    }
  }
}

impl Drop for HashWaiter {
  fn drop(&mut self) {
    // The worker itself holds no waiter slot. Consequently the transition to
    // zero means no caller can consume its result and the I/O should stop.
    if self.job.waiters.fetch_sub(1, Ordering::AcqRel) == 1 {
      self.job.cancel.cancel();
    }
  }
}

async fn run_hash_job(inner: &SchedulerInner, path: &Path, job: &HashJob) -> SharedHashResult {
  let permit = tokio::select! {
    () = job.cancel.cancelled() => return Err(SharedHashError::Cancelled),
    permit = inner.permits.clone().acquire_owned() => {
      permit.map_err(|error| SharedHashError::Worker(error.to_string()))?
    },
  };
  let options = inner.options;
  let path = path.to_path_buf();
  let cancel = job.cancel.clone();
  tokio::task::spawn_blocking(move || {
    // Keep the permit in the blocking worker so dropping the async join future
    // can never release capacity while filesystem work is still running.
    let _permit = permit;
    hash_stable_file(&path, options, &cancel)
  })
  .await
  .map_err(|error| SharedHashError::Worker(error.to_string()))?
}

#[derive(Clone, Debug)]
/// Cloneable worker failure transported to every waiter over a watch channel.
///
/// `std::io::Error` is reconstructed at the API boundary because it is not
/// cloneable, while every coalesced caller must observe an equivalent failure.
enum SharedHashError {
  Cancelled,
  Worker(String),
  Io(&'static str, std::io::ErrorKind, String),
  Unstable,
}

#[derive(Clone, Debug)]
/// Content result tied to the metadata identity validated around the read.
pub(crate) struct HashedFile {
  pub(crate) content: Digest,
  pub(crate) executable: bool,
  pub(crate) key: EntryKey,
}

impl SharedHashError {
  fn into_cache_error(self, path: &Path) -> CacheError {
    match self {
      Self::Cancelled => CacheError::Cancelled,
      Self::Worker(message) => CacheError::WorkerTerminated(message),
      Self::Io(operation, kind, message) => io_error(operation, path, std::io::Error::new(kind, message)),
      Self::Unstable => CacheError::UnstableFile {
        path: path.to_path_buf(),
      },
    }
  }
}

fn hash_stable_file(
  path: &Path,
  options: SnapshotOptions,
  cancel: &CancellationToken,
) -> Result<HashedFile, SharedHashError> {
  hash_stable_file_with_observer(path, options, cancel, |_| {})
}

fn hash_stable_file_with_observer(
  path: &Path,
  options: SnapshotOptions,
  cancel: &CancellationToken,
  mut after_chunk: impl FnMut(u64),
) -> Result<HashedFile, SharedHashError> {
  // Check both the opened handle and the path before accepting the digest. The
  // former detects mutation of the bytes we read; the latter detects atomic
  // replacement of the directory entry while that handle remained valid. The
  // snapshotter owns the single retry budget so one configured retry cannot be
  // multiplied by nested retry loops.
  if cancel.is_cancelled() {
    return Err(SharedHashError::Cancelled);
  }
  let before = fs::symlink_metadata(path).map_err(|error| shared_io("inspect cache input", error))?;
  if !before.is_file() || before.file_type().is_symlink() {
    return Err(SharedHashError::Unstable);
  }
  let before_key = EntryKey::new(path, &before).map_err(shared_cache_error)?;
  let mut file = File::open(path).map_err(|error| shared_io("open cache input", error))?;
  let opened = file
    .metadata()
    .map_err(|error| shared_io("inspect open cache input", error))?;
  if EntryKey::new(path, &opened).map_err(shared_cache_error)? != before_key {
    return Err(SharedHashError::Unstable);
  }
  let mut buffer = vec![0_u8; options.read_buffer_bytes];
  let mut hasher = blake3::Hasher::new();
  let mut bytes = 0_u64;
  loop {
    if cancel.is_cancelled() {
      return Err(SharedHashError::Cancelled);
    }
    let read = file
      .read(&mut buffer)
      .map_err(|error| shared_io("read cache input", error))?;
    if read == 0 {
      break;
    }
    hasher.update(&buffer[..read]);
    bytes = bytes
      .checked_add(read as u64)
      .ok_or_else(|| SharedHashError::Worker("hashed byte count overflowed".to_owned()))?;
    after_chunk(bytes);
  }
  let opened_after = file
    .metadata()
    .map_err(|error| shared_io("reinspect open cache input", error))?;
  let path_after = fs::symlink_metadata(path).map_err(|error| shared_io("reinspect cache input", error))?;
  if EntryKey::new(path, &opened_after).map_err(shared_cache_error)? != before_key
    || EntryKey::new(path, &path_after).map_err(shared_cache_error)? != before_key
    || bytes != before.len()
  {
    return Err(SharedHashError::Unstable);
  }
  Ok(HashedFile {
    content: Digest::new(DigestAlgorithm::Blake3, *hasher.finalize().as_bytes(), bytes),
    executable: executable(&before),
    key: before_key,
  })
}

fn shared_io(operation: &'static str, error: std::io::Error) -> SharedHashError {
  SharedHashError::Io(operation, error.kind(), error.to_string())
}

fn shared_cache_error(error: CacheError) -> SharedHashError {
  match error {
    CacheError::Io { operation, source, .. } => shared_io(operation, source),
    other => SharedHashError::Worker(other.to_string()),
  }
}

#[cfg(test)]
mod tests {
  use std::{cell::Cell, fs, time::Duration};

  use tempfile::TempDir;

  use super::*;

  async fn wait_for_waiters(scheduler: &HashScheduler, minimum: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
      loop {
        let in_flight = scheduler.inner.in_flight.lock().await;
        if in_flight
          .values()
          .next()
          .is_some_and(|job| job.waiters.load(Ordering::Acquire) >= minimum)
        {
          return;
        }
        drop(in_flight);
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("hash request did not enter the shared in-flight table");
  }

  async fn wait_until_idle(scheduler: &HashScheduler) {
    tokio::time::timeout(Duration::from_secs(5), async {
      loop {
        if scheduler.inner.in_flight.lock().await.is_empty() {
          return;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("hash worker did not leave the in-flight table");
  }

  #[tokio::test]
  async fn concurrent_waiters_share_work_and_have_independent_cancellation() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("input");
    fs::write(&path, "shared data").unwrap();
    let scheduler = HashScheduler::new(SnapshotOptions {
      max_parallel_hashes: 1,
      ..SnapshotOptions::default()
    })
    .unwrap();
    let held = scheduler.inner.permits.clone().acquire_owned().await.unwrap();
    let key = EntryKey::new(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();
    let first_cancel = CancellationToken::new();

    let first = {
      let scheduler = scheduler.clone();
      let path = path.clone();
      let key = key.clone();
      let cancel = first_cancel.clone();
      tokio::spawn(async move { scheduler.hash(&path, key, &cancel).await })
    };
    wait_for_waiters(&scheduler, 1).await;
    let second = {
      let scheduler = scheduler.clone();
      let path = path.clone();
      let key = key.clone();
      tokio::spawn(async move { scheduler.hash(&path, key, &CancellationToken::new()).await })
    };
    wait_for_waiters(&scheduler, 2).await;

    first_cancel.cancel();
    assert!(matches!(first.await.unwrap(), Err(CacheError::Cancelled)));
    assert_eq!(scheduler.inner.in_flight.lock().await.len(), 1);
    drop(held);
    assert_eq!(second.await.unwrap().unwrap().content, Digest::blake3(b"shared data"));
    wait_until_idle(&scheduler).await;
  }

  #[tokio::test]
  async fn the_last_cancelled_waiter_stops_and_removes_the_job() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("input");
    fs::write(&path, "data").unwrap();
    let scheduler = HashScheduler::new(SnapshotOptions {
      max_parallel_hashes: 1,
      ..SnapshotOptions::default()
    })
    .unwrap();
    let held = scheduler.inner.permits.clone().acquire_owned().await.unwrap();
    let key = EntryKey::new(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let task = {
      let scheduler = scheduler.clone();
      let path = path.clone();
      let cancel = cancel.clone();
      tokio::spawn(async move { scheduler.hash(&path, key, &cancel).await })
    };
    wait_for_waiters(&scheduler, 1).await;

    cancel.cancel();
    assert!(matches!(task.await.unwrap(), Err(CacheError::Cancelled)));
    wait_until_idle(&scheduler).await;
    drop(held);
  }

  #[tokio::test]
  async fn a_disappeared_worker_unblocks_waiters_and_can_be_replaced() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("input");
    fs::write(&path, "data").unwrap();
    let key = EntryKey::new(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();
    let scheduler = HashScheduler::new(SnapshotOptions::default()).unwrap();
    let (waiter, sender) = scheduler.waiter(key.clone()).await;
    let sender = sender.expect("the first waiter starts a worker");
    drop(sender);

    let error = tokio::time::timeout(Duration::from_secs(1), waiter.wait(&CancellationToken::new()))
      .await
      .expect("a closed worker channel must wake its waiter")
      .unwrap_err();
    assert!(matches!(error, CacheError::WorkerTerminated(message) if message == "hash worker ended without a result"));

    let (_, replacement) = scheduler.waiter(key).await;
    assert!(replacement.is_some());
  }

  #[test]
  fn stable_hash_reports_errors_mutation_and_chunk_cancellation() {
    let root = TempDir::new().unwrap();
    let options = SnapshotOptions::default();
    let cancel = CancellationToken::new();
    assert!(matches!(
      hash_stable_file(&root.path().join("missing"), options, &cancel),
      Err(SharedHashError::Io(..))
    ));
    assert!(matches!(
      hash_stable_file(root.path(), options, &cancel),
      Err(SharedHashError::Unstable)
    ));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
      hash_stable_file(root.path(), options, &cancelled),
      Err(SharedHashError::Cancelled)
    ));

    let path = root.path().join("input");
    fs::write(&path, vec![1_u8; 8192]).unwrap();
    let changed = Cell::new(false);
    assert!(matches!(
      hash_stable_file_with_observer(
        &path,
        SnapshotOptions {
          read_buffer_bytes: 4096,
          mutation_retries: 0,
          ..options
        },
        &cancel,
        |_| {
          if !changed.replace(true) {
            fs::write(&path, vec![2_u8; 8193]).unwrap();
          }
        },
      ),
      Err(SharedHashError::Unstable)
    ));

    fs::write(&path, vec![1_u8; 8192]).unwrap();
    let cancel = CancellationToken::new();
    assert!(matches!(
      hash_stable_file_with_observer(
        &path,
        SnapshotOptions {
          read_buffer_bytes: 4096,
          ..options
        },
        &cancel,
        |_| cancel.cancel(),
      ),
      Err(SharedHashError::Cancelled)
    ));
  }

  #[test]
  fn shared_worker_errors_preserve_their_public_meaning() {
    let path = Path::new("input");
    assert!(matches!(
      SharedHashError::Cancelled.into_cache_error(path),
      CacheError::Cancelled
    ));
    assert!(matches!(
      SharedHashError::Worker("failed".to_owned()).into_cache_error(path),
      CacheError::WorkerTerminated(message) if message == "failed"
    ));
    assert!(matches!(
      SharedHashError::Io("read", std::io::ErrorKind::Other, "failed".to_owned()).into_cache_error(path),
      CacheError::Io { operation: "read", path: error_path, source }
        if error_path == path && source.kind() == std::io::ErrorKind::Other
    ));
    assert!(matches!(
      SharedHashError::Unstable.into_cache_error(path),
      CacheError::UnstableFile { path: error_path } if error_path == path
    ));
    assert!(matches!(
      shared_cache_error(CacheError::Configuration("invalid".to_owned())),
      SharedHashError::Worker(message) if message.contains("invalid")
    ));
  }
}
