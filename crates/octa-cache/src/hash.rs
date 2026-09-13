//! Bounded, cancellation-aware hashing of stable regular files.
//!
//! `HashScheduler` owns one Rayon pool shared by every cloned snapshotter.
//! A complete filesystem snapshot is submitted as one blocking operation, and
//! the pool distributes files without creating a Tokio task, channel, and heap
//! allocation for every small file. The configured worker count is therefore
//! also the process-wide hashing concurrency bound.

use std::{
  cell::RefCell,
  fs::File,
  io::Read,
  path::{Path, PathBuf},
  sync::{Arc, OnceLock},
};

use octa_cache_protocol::{Digest, DigestAlgorithm};
use rayon::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::{
  error::io_error,
  platform::{executable, EntryKey},
  CacheError, CacheResult,
};

// BLAKE3 documents 128 KiB as the approximate lower bound at which its Rayon
// implementation starts paying for thread coordination on x86_64. Keeping the
// threshold here makes the choice explicit and leaves small-file parallelism to
// the snapshot scheduler.
const PARALLEL_CHUNK_MIN_BYTES: usize = 128 * 1024;

thread_local! {
  // Hashing runs on the scheduler's Rayon workers. Retaining one bounded
  // buffer per worker avoids allocating the default 1 MiB buffer for every
  // tiny source file while keeping buffers out of shared synchronization.
  static READ_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

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
  /// Largest serialized local input-digest memo accepted into memory.
  pub max_memo_bytes: u64,
}

impl Default for SnapshotOptions {
  fn default() -> Self {
    Self {
      max_parallel_hashes: std::thread::available_parallelism().map_or(1, usize::from),
      max_entries: 1_000_000,
      read_buffer_bytes: 1024 * 1024,
      mutation_retries: 2,
      max_memo_bytes: 512 * 1024 * 1024,
    }
  }
}

impl SnapshotOptions {
  fn validate(self) -> CacheResult<Self> {
    if self.max_parallel_hashes == 0 || self.max_entries == 0 || self.max_memo_bytes == 0 {
      return Err(CacheError::Configuration(
        "max_parallel_hashes, max_entries, and max_memo_bytes must be greater than zero".to_owned(),
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

/// Runtime-wide bounded file-hashing pool.
#[derive(Clone)]
pub(crate) struct HashScheduler {
  inner: Arc<SchedulerInner>,
}

/// State shared by every snapshotter clone in this process.
struct SchedulerInner {
  options: SnapshotOptions,
  // Cache hits resolved by the metadata memo never need hashing threads. Keep
  // pool creation off both non-cache runs and that hot path.
  pool: OnceLock<Result<rayon::ThreadPool, String>>,
}

impl HashScheduler {
  pub(crate) fn new(options: SnapshotOptions) -> CacheResult<Self> {
    let options = options.validate()?;
    Ok(Self {
      inner: Arc::new(SchedulerInner {
        options,
        pool: OnceLock::new(),
      }),
    })
  }

  pub(crate) fn options(&self) -> SnapshotOptions {
    self.inner.options
  }

  pub(crate) async fn hash(&self, path: &Path, key: EntryKey, cancel: &CancellationToken) -> CacheResult<HashedFile> {
    let mut result = self.hash_many(vec![(path.to_path_buf(), key)], cancel).await;
    result.pop().expect("one hash request produces one result")
  }

  /// Hashes a complete discovered file set on the shared fixed-size pool.
  ///
  /// Rayon uses indexed parallel iteration, so the returned result at each
  /// position always belongs to the request at the same position. Large-file
  /// chunk parallelism is enabled only for a single request; otherwise files
  /// themselves are the parallel work units and nested pools are avoided.
  pub(crate) async fn hash_many(
    &self,
    requests: Vec<(PathBuf, EntryKey)>,
    cancel: &CancellationToken,
  ) -> Vec<CacheResult<HashedFile>> {
    if requests.is_empty() {
      return Vec::new();
    }
    let inner = self.inner.clone();
    let cancel = cancel.clone();
    let parallel_chunks = requests.len() == 1;
    let paths = requests.iter().map(|(path, _)| path.clone()).collect::<Vec<_>>();
    let result = tokio::task::spawn_blocking(move || {
      let pool = inner.pool.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
          .num_threads(inner.options.max_parallel_hashes)
          .thread_name(|index| format!("octa-cache-hash-{index}"))
          .build()
          .map_err(|error| error.to_string())
      });
      let pool = pool.as_ref().map_err(Clone::clone)?;
      Ok::<_, String>(pool.install(|| {
        requests
          .into_par_iter()
          .map(|(path, key)| hash_stable_file_with_retries(&path, key, inner.options, &cancel, parallel_chunks, |_| {}))
          .collect::<Vec<_>>()
      }))
    })
    .await;
    match result {
      Ok(Ok(results)) => results
        .into_iter()
        .zip(paths)
        .map(|(result, path)| result.map_err(|error| error.into_cache_error(&path)))
        .collect(),
      Ok(Err(error)) => paths
        .into_iter()
        .map(|_| {
          Err(CacheError::WorkerTerminated(format!(
            "failed to create cache hashing pool: {error}"
          )))
        })
        .collect(),
      Err(error) => paths
        .into_iter()
        .map(|_| Err(CacheError::WorkerTerminated(error.to_string())))
        .collect(),
    }
  }
}

#[derive(Debug)]
/// Sendable worker failure converted back to a path-aware cache error after
/// the blocking batch rejoins the async runtime.
enum WorkerHashError {
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

impl WorkerHashError {
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

#[cfg(test)]
fn hash_stable_file(
  path: &Path,
  options: SnapshotOptions,
  cancel: &CancellationToken,
) -> Result<HashedFile, WorkerHashError> {
  hash_stable_file_with_observer(path, options, cancel, |_| {})
}

#[cfg(test)]
fn hash_stable_file_with_observer(
  path: &Path,
  options: SnapshotOptions,
  cancel: &CancellationToken,
  after_chunk: impl FnMut(u64),
) -> Result<HashedFile, WorkerHashError> {
  if cancel.is_cancelled() {
    return Err(WorkerHashError::Cancelled);
  }
  let before = std::fs::symlink_metadata(path).map_err(|error| shared_io("inspect cache input", error))?;
  if !before.is_file() || before.file_type().is_symlink() {
    return Err(WorkerHashError::Unstable);
  }
  let key = EntryKey::new(path, &before).map_err(shared_cache_error)?;
  hash_stable_file_with_retries(path, key, options, cancel, true, after_chunk)
}

/// Applies one retry budget around stable reads without leaving the Rayon pool.
///
/// A retry refreshes only the file that changed. Stable snapshots take the
/// single-attempt path, while several unstable files remain independent Rayon
/// work units instead of falling back to sequential async retries.
fn hash_stable_file_with_retries(
  path: &Path,
  mut expected: EntryKey,
  options: SnapshotOptions,
  cancel: &CancellationToken,
  parallel_chunks: bool,
  mut after_chunk: impl FnMut(u64),
) -> Result<HashedFile, WorkerHashError> {
  for attempt in 0..=options.mutation_retries {
    match hash_stable_file_for_key(path, &expected, options, cancel, parallel_chunks, &mut after_chunk) {
      Err(WorkerHashError::Unstable) if attempt < options.mutation_retries => {
        let metadata =
          std::fs::symlink_metadata(path).map_err(|error| shared_io("reinspect changed cache input", error))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
          return Err(WorkerHashError::Unstable);
        }
        expected = EntryKey::new(path, &metadata).map_err(shared_cache_error)?;
      },
      outcome => return outcome,
    }
  }
  Err(WorkerHashError::Unstable)
}

fn hash_stable_file_for_key(
  path: &Path,
  expected: &EntryKey,
  options: SnapshotOptions,
  cancel: &CancellationToken,
  parallel_chunks: bool,
  mut after_chunk: impl FnMut(u64),
) -> Result<HashedFile, WorkerHashError> {
  // Inspect the opened handle after reading. Comparing that handle with the
  // discovery key detects opening a replacement and mutation during the read
  // with one fstat per file. Cache-hit restoration separately revalidates the
  // current paths, so repeating a path stat here would not close a race that
  // can occur immediately after this function returns.
  if cancel.is_cancelled() {
    return Err(WorkerHashError::Cancelled);
  }
  let mut file = File::open(path).map_err(|error| shared_io("open cache input", error))?;
  let mut hasher = blake3::Hasher::new();
  let mut bytes = 0_u64;
  READ_BUFFER.with(|buffer| -> Result<(), WorkerHashError> {
    let mut buffer = buffer.borrow_mut();
    buffer.resize(options.read_buffer_bytes, 0);
    loop {
      if cancel.is_cancelled() {
        return Err(WorkerHashError::Cancelled);
      }
      let read = file
        .read(&mut buffer)
        .map_err(|error| shared_io("read cache input", error))?;
      if read == 0 {
        break;
      }
      if parallel_chunks && read >= PARALLEL_CHUNK_MIN_BYTES {
        hasher.update_rayon(&buffer[..read]);
      } else {
        hasher.update(&buffer[..read]);
      }
      bytes = bytes
        .checked_add(read as u64)
        .ok_or_else(|| WorkerHashError::Worker("hashed byte count overflowed".to_owned()))?;
      after_chunk(bytes);
    }
    Ok(())
  })?;
  let opened = file
    .metadata()
    .map_err(|error| shared_io("reinspect open cache input", error))?;
  let opened_key = EntryKey::new(path, &opened).map_err(shared_cache_error)?;
  if !opened.is_file() || &opened_key != expected || bytes != expected.length() {
    return Err(WorkerHashError::Unstable);
  }
  Ok(HashedFile {
    content: Digest::new(DigestAlgorithm::Blake3, *hasher.finalize().as_bytes(), bytes),
    executable: executable(&opened),
    key: opened_key,
  })
}

fn shared_io(operation: &'static str, error: std::io::Error) -> WorkerHashError {
  WorkerHashError::Io(operation, error.kind(), error.to_string())
}

fn shared_cache_error(error: CacheError) -> WorkerHashError {
  match error {
    CacheError::Io { operation, source, .. } => shared_io(operation, source),
    other => WorkerHashError::Worker(other.to_string()),
  }
}

#[cfg(test)]
mod tests {
  use std::{cell::Cell, fs};

  use tempfile::TempDir;

  use super::*;

  #[tokio::test]
  async fn bulk_hashing_preserves_request_order_and_cancellation() {
    let root = TempDir::new().unwrap();
    let scheduler = HashScheduler::new(SnapshotOptions {
      max_parallel_hashes: 2,
      ..SnapshotOptions::default()
    })
    .unwrap();
    let mut requests = Vec::new();
    for (name, contents) in [("first", b"one".as_slice()), ("second", b"two".as_slice())] {
      let path = root.path().join(name);
      fs::write(&path, contents).unwrap();
      let key = EntryKey::new(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();
      requests.push((path, key));
    }
    let results = scheduler.hash_many(requests.clone(), &CancellationToken::new()).await;
    assert_eq!(results[0].as_ref().unwrap().content, Digest::blake3(b"one"));
    assert_eq!(results[1].as_ref().unwrap().content, Digest::blake3(b"two"));
    assert_eq!(
      scheduler
        .inner
        .pool
        .get()
        .expect("hashing initializes the shared pool")
        .as_ref()
        .unwrap()
        .current_num_threads(),
      2
    );
    assert!(Arc::ptr_eq(&scheduler.inner, &scheduler.clone().inner));

    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(scheduler
      .hash_many(requests, &cancel)
      .await
      .into_iter()
      .all(|result| matches!(result, Err(CacheError::Cancelled))));
  }

  #[tokio::test]
  async fn single_large_file_uses_the_same_bounded_scheduler_api() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("large-input");
    let contents = vec![42_u8; PARALLEL_CHUNK_MIN_BYTES * 2];
    fs::write(&path, &contents).unwrap();
    let key = EntryKey::new(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();

    let hashed = HashScheduler::new(SnapshotOptions {
      max_parallel_hashes: 2,
      ..SnapshotOptions::default()
    })
    .unwrap()
    .hash(&path, key, &CancellationToken::new())
    .await
    .unwrap();

    assert_eq!(hashed.content, Digest::blake3(&contents));
  }

  #[test]
  fn stable_hash_reports_errors_mutation_and_chunk_cancellation() {
    let root = TempDir::new().unwrap();
    let options = SnapshotOptions::default();
    let cancel = CancellationToken::new();
    assert!(matches!(
      hash_stable_file(&root.path().join("missing"), options, &cancel),
      Err(WorkerHashError::Io(..))
    ));
    assert!(matches!(
      hash_stable_file(root.path(), options, &cancel),
      Err(WorkerHashError::Unstable)
    ));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
      hash_stable_file(root.path(), options, &cancelled),
      Err(WorkerHashError::Cancelled)
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
      Err(WorkerHashError::Unstable)
    ));

    let replacement = vec![3_u8; 8193];
    fs::write(&path, vec![1_u8; 8192]).unwrap();
    let changed = Cell::new(false);
    let hashed = hash_stable_file_with_observer(
      &path,
      SnapshotOptions {
        read_buffer_bytes: 4096,
        mutation_retries: 1,
        ..options
      },
      &cancel,
      |_| {
        if !changed.replace(true) {
          fs::write(&path, &replacement).unwrap();
        }
      },
    )
    .unwrap();
    assert_eq!(hashed.content, Digest::blake3(&replacement));
    assert_eq!(hashed.key.length(), replacement.len() as u64);

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
      Err(WorkerHashError::Cancelled)
    ));
  }

  #[test]
  fn shared_worker_errors_preserve_their_public_meaning() {
    let path = Path::new("input");
    assert!(matches!(
      WorkerHashError::Cancelled.into_cache_error(path),
      CacheError::Cancelled
    ));
    assert!(matches!(
      WorkerHashError::Worker("failed".to_owned()).into_cache_error(path),
      CacheError::WorkerTerminated(message) if message == "failed"
    ));
    assert!(matches!(
      WorkerHashError::Io("read", std::io::ErrorKind::Other, "failed".to_owned()).into_cache_error(path),
      CacheError::Io { operation: "read", path: error_path, source }
        if error_path == path && source.kind() == std::io::ErrorKind::Other
    ));
    assert!(matches!(
      WorkerHashError::Unstable.into_cache_error(path),
      CacheError::UnstableFile { path: error_path } if error_path == path
    ));
    assert!(matches!(
      shared_cache_error(CacheError::Configuration("invalid".to_owned())),
      WorkerHashError::Worker(message) if message.contains("invalid")
    ));
  }
}
