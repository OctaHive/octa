//! Crash-recoverable replacement of declared task outputs.
//!
//! Bundle extraction happens in a sibling of the workspace so every final move
//! remains on one filesystem. An immutable intent journal makes even abandoned
//! staging discoverable. A durable prepared marker is published only after the
//! bundle is fully verified, and a later commit marker distinguishes rollback
//! from completed replacements whose backups only need cleanup.

mod journal;

use std::{
  collections::BTreeSet,
  fs::{self, File, OpenOptions},
  io::{self, Read},
  path::{Path, PathBuf},
};

use octa_cache_protocol::{BlobDescriptor, RelativePath};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
  bundle::validate_output_roots,
  error::{check_cancelled, io_error},
  extract_bundle, inspect_outputs,
  locking::shard_name,
  platform::sync_directory,
  workspace::join_relative,
  BundleLimits, CacheError, CacheResult,
};
use journal::{
  cleanup_temporary_journals, commit_marker, journal_files, marker_exists, prepared_marker, read_journal,
  remove_transaction_state, rolled_back_marker, transaction_path, validate_journal, write_commit_marker, write_journal,
  write_prepared_marker, write_rolled_back_marker, JournalRoot, RestoreJournal,
};

/// Result of a verified output restore request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreOutcome {
  /// Live outputs already had the bundle's exact canonical identity.
  AlreadyMaterialized,
  /// The bundle was staged, verified, and committed to the workspace.
  Restored,
}

/// Cross-process exclusive ownership of a set of output roots and prefixes.
///
/// Capturing code keeps this guard alive while packing outputs; restore uses
/// the same lock domain so another Octa process cannot capture a half-replaced
/// tree.
pub struct OutputLockGuard {
  _files: Vec<File>,
}

/// Owns output locks and durable recovery journals for local restoration.
#[derive(Clone, Debug)]
pub struct RestoreManager {
  state_root: PathBuf,
  limits: BundleLimits,
}

/// Policy hooks kept outside the restore state machine's durable data.
struct RestoreCallbacks<V, O> {
  validate: V,
  durable_stage: O,
}

impl RestoreManager {
  /// Opens restore state below an initialized versioned local-cache directory.
  pub fn open(cache_layout_root: impl Into<PathBuf>, limits: BundleLimits) -> CacheResult<Self> {
    let state_root = cache_layout_root.into();
    let journal_root = state_root.join("restore-journal");
    let lock_root = state_root.join("locks/outputs");
    fs::create_dir_all(&journal_root)
      .map_err(|error| io_error("create restore journal directory", &journal_root, error))?;
    fs::create_dir_all(&lock_root).map_err(|error| io_error("create output lock directory", &lock_root, error))?;
    Ok(Self { state_root, limits })
  }

  /// Restores a verified bundle without ever extracting over live outputs.
  ///
  /// Equal current outputs are left untouched. Otherwise this method locks all
  /// path prefixes, extracts into staging, records the rollback information,
  /// replaces each root, and marks the transaction committed before cleanup.
  pub fn restore<R: Read>(
    &self,
    reader: R,
    descriptor: &BlobDescriptor,
    workspace: &Path,
    output_roots: &[RelativePath],
    cancel: &CancellationToken,
  ) -> CacheResult<RestoreOutcome> {
    self.restore_validated(reader, descriptor, workspace, output_roots, cancel, |_| Ok(()))
  }

  /// Restores outputs only when `validate` accepts the complete materialization.
  ///
  /// Validation runs against either the already materialized workspace or the
  /// fully decoded staging tree before any live output is replaced. This lets
  /// higher layers enforce result metadata contracts without weakening the
  /// restore transaction or teaching this crate about artifacts and reports.
  pub fn restore_validated<R: Read>(
    &self,
    reader: R,
    descriptor: &BlobDescriptor,
    workspace: &Path,
    output_roots: &[RelativePath],
    cancel: &CancellationToken,
    validate: impl Fn(&Path) -> CacheResult<()>,
  ) -> CacheResult<RestoreOutcome> {
    let _active_restore = self.lock_recovery(false)?;
    self.restore_with_callbacks(
      reader,
      descriptor,
      workspace,
      output_roots,
      cancel,
      RestoreCallbacks {
        validate,
        durable_stage: |_| {},
      },
    )
  }

  /// Locks every declared output and its relative prefixes in stable order.
  pub fn lock_outputs(&self, workspace: &Path, output_roots: &[RelativePath]) -> CacheResult<OutputLockGuard> {
    let output_roots = validate_output_roots(output_roots)?;
    Ok(OutputLockGuard {
      _files: self.lock_output_paths(workspace, &output_roots)?,
    })
  }

  /// Recovers every journal left by a terminated Octa process.
  ///
  /// Prepared transactions are rolled back. Committed transactions retain the
  /// new outputs and discard only staging and backups.
  pub fn recover(&self) -> CacheResult<usize> {
    // Only one process may consume the journal directory at a time. Output
    // locks alone are insufficient because two recovery scans can both retain
    // the same journal before either process removes it. Active restores hold
    // the shared side, so recovery also cannot mistake a live journal for one
    // abandoned by a terminated process.
    let _recovery_lock = self.lock_recovery(true)?;
    cleanup_temporary_journals(&self.journal_root())?;
    let mut recovered = 0;
    for path in journal_files(&self.journal_root())? {
      let journal = read_journal(&path)?;
      if !recovery_workspace_exists(&journal.workspace)? {
        remove_transaction_state(&path, &journal.transaction)?;
        recovered += 1;
        continue;
      }
      let _locks = self.lock_journal_outputs(&journal.workspace, &journal.roots)?;
      self.recover_one(&path, &journal)?;
      recovered += 1;
    }
    Ok(recovered)
  }

  #[cfg(test)]
  fn restore_with_observer<R: Read>(
    &self,
    reader: R,
    descriptor: &BlobDescriptor,
    workspace: &Path,
    output_roots: &[RelativePath],
    cancel: &CancellationToken,
    durable_stage: impl FnMut(&'static str),
  ) -> CacheResult<RestoreOutcome> {
    let _active_restore = self.lock_recovery(false)?;
    self.restore_with_callbacks(
      reader,
      descriptor,
      workspace,
      output_roots,
      cancel,
      RestoreCallbacks {
        validate: |_: &Path| Ok(()),
        durable_stage,
      },
    )
  }

  fn restore_with_callbacks<R: Read>(
    &self,
    reader: R,
    descriptor: &BlobDescriptor,
    workspace: &Path,
    output_roots: &[RelativePath],
    cancel: &CancellationToken,
    callbacks: RestoreCallbacks<impl Fn(&Path) -> CacheResult<()>, impl FnMut(&'static str)>,
  ) -> CacheResult<RestoreOutcome> {
    let RestoreCallbacks {
      validate,
      mut durable_stage,
    } = callbacks;
    // Durable state advances monotonically:
    // intent -> verified staging/prepared -> live replacements/committed.
    // Any failure before `prepared` only discards staging; any failure after it
    // rolls live roots back from their per-root backups. The observer exists
    // solely for crash-boundary tests and cannot influence the transition.
    descriptor.validate()?;
    let workspace =
      dunce::canonicalize(workspace).map_err(|error| io_error("canonicalize restore workspace", workspace, error))?;
    let output_roots = validate_output_roots(output_roots)?;
    let _locks = self.lock_output_paths(&workspace, &output_roots)?;
    check_cancelled(cancel)?;
    if outputs_match(&workspace, &output_roots, descriptor, self.limits, cancel)? {
      validate(&workspace)?;
      return Ok(RestoreOutcome::AlreadyMaterialized);
    }

    let id = Uuid::new_v4().to_string();
    let transaction = transaction_path(&workspace, &id)?;
    let (entries, created_parents) = inspect_live_roots(&workspace, &output_roots)?;
    let journal = RestoreJournal::new(id, workspace.clone(), transaction.clone(), entries, created_parents);
    let journal_path = self.journal_root().join(format!("{}.json", journal.id));
    write_journal(&journal_path, &journal)?;
    durable_stage("restore_intent_recorded");

    let staging = transaction.join("staging");
    let backup = transaction.join("backup");
    let preparation = (|| -> CacheResult<()> {
      fs::create_dir(&transaction).map_err(|error| io_error("create restore transaction", &transaction, error))?;
      fs::create_dir(&staging).map_err(|error| io_error("create restore staging directory", &staging, error))?;
      fs::create_dir(&backup).map_err(|error| io_error("create restore backup directory", &backup, error))?;
      extract_bundle(reader, descriptor, &staging, &output_roots, self.limits, cancel)?;
      validate(&staging)?;
      // Backup parent directories are part of the prepared state. Creating
      // them after the marker could make a subsequent live-to-backup rename
      // durable while its destination directory itself was not.
      for entry in journal.roots.iter().filter(|entry| entry.had_original) {
        create_parent(&join_relative(&backup, &entry.path))?;
      }
      sync_tree_directories(&transaction)?;
      sync_directory(transaction.parent().expect("a restore transaction has a parent"))?;
      write_prepared_marker(&journal_path)
    })();
    if let Err(error) = preparation {
      remove_transaction_state(&journal_path, &transaction)?;
      return Err(error);
    }
    durable_stage("journal_prepared");

    let mutation = (|| -> CacheResult<()> {
      create_live_parents(&workspace, &journal.created_parents)?;
      for entry in &journal.roots {
        check_cancelled(cancel)?;
        let live = join_relative(&workspace, &entry.path);
        let saved = join_relative(&backup, &entry.path);
        let staged = join_relative(&staging, &entry.path);
        if entry.had_original {
          create_parent(&saved)?;
          fs::rename(&live, &saved).map_err(|error| io_error("move live output to restore backup", &live, error))?;
          sync_rename_parents(&live, &saved)?;
          durable_stage("original_backed_up");
        }
        create_parent(&live)?;
        fs::rename(&staged, &live).map_err(|error| io_error("install staged cache output", &live, error))?;
        sync_rename_parents(&staged, &live)?;
        durable_stage("staged_output_installed");
      }
      write_commit_marker(&journal_path)?;
      durable_stage("transaction_committed");
      Ok(())
    })();

    if let Err(error) = mutation {
      self.rollback(&journal_path, &journal)?;
      return Err(error);
    }
    self.cleanup_committed(&journal_path, &journal)?;
    Ok(RestoreOutcome::Restored)
  }

  fn recover_one(&self, path: &Path, journal: &RestoreJournal) -> CacheResult<()> {
    validate_journal(journal)?;
    let committed = marker_exists(&commit_marker(path))?;
    let rolled_back = marker_exists(&rolled_back_marker(path))?;
    let prepared = marker_exists(&prepared_marker(path))?;
    if committed && rolled_back {
      return Err(CacheError::Metadata(format!(
        "restore transaction '{}' has conflicting terminal markers",
        journal.id
      )));
    }
    if committed {
      self.cleanup_committed(path, journal)
    } else if rolled_back {
      remove_transaction_state(path, &journal.transaction)
    } else if prepared {
      self.rollback(path, journal)
    } else {
      // Without the prepared marker no live output was touched; only staging
      // and the intent journal can exist.
      remove_transaction_state(path, &journal.transaction)
    }
  }

  fn rollback(&self, journal_path: &Path, journal: &RestoreJournal) -> CacheResult<()> {
    let staging = journal.transaction.join("staging");
    let backup = journal.transaction.join("backup");
    for entry in journal.roots.iter().rev() {
      let live = join_relative(&journal.workspace, &entry.path);
      let staged = join_relative(&staging, &entry.path);
      let saved = join_relative(&backup, &entry.path);
      // Roots are processed backwards because installation follows declaration
      // order. Moving the new live value back into staging, rather than deleting
      // it, preserves enough state to repeat rollback after a crash.
      if entry.had_original {
        if path_exists(&saved)? {
          if path_exists(&live)? {
            if path_exists(&staged)? {
              return Err(CacheError::Metadata(format!(
                "restore transaction '{}' found both staged and live output '{}'",
                journal.id, entry.path
              )));
            }
            create_parent(&staged)?;
            fs::rename(&live, &staged)
              .map_err(|error| io_error("return installed output to restore staging", &live, error))?;
            sync_rename_parents(&live, &staged)?;
          }
          create_parent(&live)?;
          fs::rename(&saved, &live).map_err(|error| io_error("restore backed-up output", &saved, error))?;
          sync_rename_parents(&saved, &live)?;
        } else if !path_exists(&staged)? {
          return Err(CacheError::Metadata(format!(
            "restore transaction '{}' lost both staged and backup state for '{}'",
            journal.id, entry.path
          )));
        }
      } else if !path_exists(&staged)? {
        if !path_exists(&live)? {
          return Err(CacheError::Metadata(format!(
            "restore transaction '{}' lost both staged and live output '{}'",
            journal.id, entry.path
          )));
        }
        create_parent(&staged)?;
        fs::rename(&live, &staged).map_err(|error| io_error("return new output to restore staging", &live, error))?;
        sync_rename_parents(&live, &staged)?;
      } else if path_exists(&live)? {
        return Err(CacheError::Metadata(format!(
          "restore transaction '{}' found an unexpected live output '{}'",
          journal.id, entry.path
        )));
      }
    }
    for parent in journal.created_parents.iter().rev() {
      let path = join_relative(&journal.workspace, parent);
      match fs::remove_dir(&path) {
        Ok(()) => sync_parent(&path)?,
        Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) => {},
        Err(error) => return Err(io_error("remove restore-created parent", path, error)),
      }
    }
    write_rolled_back_marker(journal_path)?;
    remove_transaction_state(journal_path, &journal.transaction)
  }

  fn cleanup_committed(&self, journal_path: &Path, journal: &RestoreJournal) -> CacheResult<()> {
    for entry in &journal.roots {
      let live = join_relative(&journal.workspace, &entry.path);
      if !path_exists(&live)? {
        return Err(CacheError::Metadata(format!(
          "committed restore transaction '{}' has no live output '{}'",
          journal.id, entry.path
        )));
      }
    }
    remove_transaction_state(journal_path, &journal.transaction)
  }

  fn lock_journal_outputs(&self, workspace: &Path, roots: &[JournalRoot]) -> CacheResult<Vec<File>> {
    self.lock_output_paths(workspace, roots.iter().map(|root| &root.path))
  }

  fn lock_output_paths<'a>(
    &self,
    workspace: &Path,
    roots: impl IntoIterator<Item = &'a RelativePath>,
  ) -> CacheResult<Vec<File>> {
    let mut keys = BTreeSet::new();
    for root in roots {
      let mut prefix = String::new();
      for component in root.as_str().split('/') {
        if !prefix.is_empty() {
          prefix.push('/');
        }
        prefix.push_str(component);
        keys.insert(prefix.clone());
      }
    }
    let workspace_key = dunce::canonicalize(workspace)
      .map_err(|error| io_error("canonicalize output lock workspace", workspace, error))?;
    // Hash logical prefix keys into a bounded stripe set, then sort the final
    // lock paths. Sorting after hashing prevents lock-order inversions when two
    // unrelated prefixes collide with different stripes.
    let stripes = keys
      .into_iter()
      .map(|key| {
        let mut bytes = workspace_key.as_os_str().as_encoded_bytes().to_vec();
        bytes.push(0);
        bytes.extend_from_slice(key.as_bytes());
        shard_name(&bytes)
      })
      .collect::<BTreeSet<_>>();
    let mut locks = Vec::with_capacity(stripes.len());
    for stripe in stripes {
      let path = self.state_root.join("locks/outputs").join(format!("{stripe}.lock"));
      let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| io_error("open output lock", &path, error))?;
      fs2::FileExt::lock_exclusive(&file).map_err(|error| io_error("lock task output", &path, error))?;
      locks.push(file);
    }
    Ok(locks)
  }

  fn lock_recovery(&self, exclusive: bool) -> CacheResult<File> {
    let path = self.state_root.join("locks/restore-recovery.lock");
    let file = OpenOptions::new()
      .create(true)
      .truncate(false)
      .read(true)
      .write(true)
      .open(&path)
      .map_err(|error| io_error("open restore recovery lock", &path, error))?;
    if exclusive {
      fs2::FileExt::lock_exclusive(&file).map_err(|error| io_error("lock restore recovery", &path, error))?;
    } else {
      fs2::FileExt::lock_shared(&file).map_err(|error| io_error("register active restore", &path, error))?;
    }
    Ok(file)
  }

  fn journal_root(&self) -> PathBuf {
    self.state_root.join("restore-journal")
  }
}

/// Rejects a replaced or redirected workspace before journal replay.
fn recovery_workspace_exists(workspace: &Path) -> CacheResult<bool> {
  let metadata = match fs::symlink_metadata(workspace) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
    Err(error) => return Err(io_error("inspect restore workspace during recovery", workspace, error)),
  };
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err(CacheError::Path {
      path: workspace.to_path_buf(),
      reason: "restore workspace was replaced by a non-directory or symbolic link".to_owned(),
    });
  }
  let canonical = dunce::canonicalize(workspace)
    .map_err(|error| io_error("canonicalize restore workspace during recovery", workspace, error))?;
  if canonical != workspace {
    return Err(CacheError::Path {
      path: workspace.to_path_buf(),
      reason: "restore workspace no longer matches its canonical journal path".to_owned(),
    });
  }
  Ok(true)
}

fn outputs_match(
  workspace: &Path,
  roots: &[RelativePath],
  expected: &BlobDescriptor,
  limits: BundleLimits,
  cancel: &CancellationToken,
) -> CacheResult<bool> {
  match inspect_outputs(workspace, roots, limits, cancel) {
    Ok(actual) => Ok(
      actual.digest == expected.digest
        && actual.expanded_size_bytes == expected.expanded_size_bytes
        && actual.entry_count == expected.entry_count,
    ),
    Err(CacheError::Cancelled) => Err(CacheError::Cancelled),
    // Missing, changed, or unsupported live outputs are not a hit. The staged
    // decoder and transactional replacement still enforce the stored bundle.
    Err(_) => Ok(false),
  }
}

fn inspect_live_roots(workspace: &Path, roots: &[RelativePath]) -> CacheResult<(Vec<JournalRoot>, Vec<RelativePath>)> {
  let mut entries = Vec::with_capacity(roots.len());
  let mut created = BTreeSet::new();
  for root in roots {
    validate_live_ancestors(workspace, root, &mut created)?;
    entries.push(JournalRoot {
      path: root.clone(),
      had_original: path_exists(&join_relative(workspace, root))?,
    });
  }
  Ok((entries, created.into_iter().collect()))
}

fn validate_live_ancestors(
  workspace: &Path,
  root: &RelativePath,
  missing: &mut BTreeSet<RelativePath>,
) -> CacheResult<()> {
  let components = root.as_str().split('/').collect::<Vec<_>>();
  let mut relative = String::new();
  for component in components.iter().take(components.len().saturating_sub(1)) {
    if !relative.is_empty() {
      relative.push('/');
    }
    relative.push_str(component);
    let path = workspace.join(&relative);
    match fs::symlink_metadata(&path) {
      Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {},
      Ok(_) => {
        return Err(CacheError::Path {
          path,
          reason: "output ancestors must be real directories, not files or symlinks".to_owned(),
        })
      },
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        // `relative` is assembled exclusively from a validated portable root,
        // so no host-path conversion or lossy normalization is needed here.
        missing.insert(RelativePath::new(relative.clone())?);
      },
      Err(error) => return Err(io_error("inspect output ancestor", path, error)),
    }
  }
  Ok(())
}

fn create_live_parents(workspace: &Path, parents: &[RelativePath]) -> CacheResult<()> {
  for parent in parents {
    let path = join_relative(workspace, parent);
    match fs::create_dir(&path) {
      Ok(()) => sync_parent(&path)?,
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
      Err(error) => return Err(io_error("create output parent", path, error)),
    }
  }
  Ok(())
}

fn path_exists(path: &Path) -> CacheResult<bool> {
  match fs::symlink_metadata(path) {
    Ok(_) => Ok(true),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(io_error("inspect restore path", path, error)),
  }
}

fn create_parent(path: &Path) -> CacheResult<()> {
  let parent = path
    .parent()
    .ok_or_else(|| CacheError::Configuration("restore path has no parent".to_owned()))?;
  fs::create_dir_all(parent).map_err(|error| io_error("create restore path parent", parent, error))
}

fn remove_file_if_present(path: &Path) -> CacheResult<()> {
  match fs::remove_file(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(io_error("remove restore state file", path, error)),
  }
}

fn sync_parent(path: &Path) -> CacheResult<()> {
  let parent = path
    .parent()
    .ok_or_else(|| CacheError::Configuration("restore path has no parent".to_owned()))?;
  sync_directory(parent)
}

/// Makes both directory-entry changes of a same-filesystem rename durable.
fn sync_rename_parents(source: &Path, destination: &Path) -> CacheResult<()> {
  sync_parent(source)?;
  if source.parent() != destination.parent() {
    sync_parent(destination)?;
  }
  Ok(())
}

fn sync_tree_directories(root: &Path) -> CacheResult<()> {
  let mut directories = vec![root.to_path_buf()];
  let mut index = 0;
  while index < directories.len() {
    for entry in fs::read_dir(&directories[index])
      .map_err(|error| io_error("scan staged restore directory", &directories[index], error))?
    {
      let entry = entry.map_err(|error| io_error("read staged restore entry", &directories[index], error))?;
      if entry
        .file_type()
        .map_err(|error| io_error("inspect staged restore entry", entry.path(), error))?
        .is_dir()
      {
        directories.push(entry.path());
      }
    }
    index += 1;
  }
  for directory in directories.into_iter().rev() {
    sync_directory(&directory)?;
  }
  Ok(())
}

#[cfg(test)]
mod tests;
