//! Durable restore journal representation and filesystem lifecycle.
//!
//! The journal is an intent record written before staging exists. A sibling
//! `.prepared` marker means extraction is durable and live-output mutation may
//! have begun; without it recovery can discard the untouched transaction. A
//! sibling `.committed` marker means every new output is installed, so recovery
//! keeps live outputs and removes only backups and transaction state. Markers
//! are separate create-only files so each state transition is atomic and can be
//! synchronized independently.

use std::{
  fs::{self, OpenOptions},
  io::{self, Write},
  path::{Path, PathBuf},
};

use octa_cache_protocol::{RelativePath, MAX_CACHE_LIST_ITEMS};
use serde::{Deserialize, Serialize};

use super::remove_file_if_present;
use crate::{bundle::validate_output_roots, error::io_error, platform::sync_directory, CacheError, CacheResult};

const JOURNAL_VERSION: u16 = 1;
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TRANSACTION_ID_BYTES: usize = 64;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Persisted information sufficient to finish or undo one interrupted restore.
pub(super) struct RestoreJournal {
  /// Journal schema version, independent of the output-bundle format.
  pub(super) version: u16,
  /// Unique transaction identifier used to derive sibling paths.
  pub(super) id: String,
  /// Canonical workspace whose declared outputs are being replaced.
  pub(super) workspace: PathBuf,
  /// Same-filesystem staging and backup directory derived from `workspace`.
  pub(super) transaction: PathBuf,
  /// Declared output roots and their state before the transaction began.
  pub(super) roots: Vec<JournalRoot>,
  /// Missing ancestors created solely to install the restored roots.
  pub(super) created_parents: Vec<RelativePath>,
}

impl RestoreJournal {
  pub(super) fn new(
    id: String,
    workspace: PathBuf,
    transaction: PathBuf,
    roots: Vec<JournalRoot>,
    created_parents: Vec<RelativePath>,
  ) -> Self {
    Self {
      version: JOURNAL_VERSION,
      id,
      workspace,
      transaction,
      roots,
      created_parents,
    }
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Pre-transaction state of one declared output root.
pub(super) struct JournalRoot {
  /// Portable path relative to the journal's workspace.
  pub(super) path: RelativePath,
  /// Whether rollback must restore an original value from backup.
  pub(super) had_original: bool,
}

/// Durably publishes the initial intent using write-sync-rename-sync ordering.
pub(super) fn write_journal(path: &Path, journal: &RestoreJournal) -> CacheResult<()> {
  validate_journal(journal)?;
  let bytes = serde_json::to_vec(journal).map_err(|error| CacheError::Metadata(error.to_string()))?;
  if bytes.len() as u64 > MAX_JOURNAL_BYTES {
    return Err(CacheError::Limit(format!(
      "restore journal exceeds {MAX_JOURNAL_BYTES} bytes"
    )));
  }
  let temporary = path.with_extension("tmp");
  let mut file = OpenOptions::new()
    .create_new(true)
    .write(true)
    .open(&temporary)
    .map_err(|error| io_error("create restore journal", &temporary, error))?;
  file
    .write_all(&bytes)
    .and_then(|()| file.sync_all())
    .map_err(|error| io_error("write restore journal", &temporary, error))?;
  fs::rename(&temporary, path).map_err(|error| io_error("publish restore journal", path, error))?;
  sync_directory(path.parent().expect("journal has a parent"))
}

/// Records the point after which installed outputs must be retained.
pub(super) fn write_commit_marker(journal: &Path) -> CacheResult<()> {
  write_marker(&commit_marker(journal))
}

/// Records the point after which recovery must inspect or roll back mutation.
pub(super) fn write_prepared_marker(journal: &Path) -> CacheResult<()> {
  write_marker(&prepared_marker(journal))
}

/// Records that rollback completed and only transaction cleanup remains.
pub(super) fn write_rolled_back_marker(journal: &Path) -> CacheResult<()> {
  write_marker(&rolled_back_marker(journal))
}

fn write_marker(marker: &Path) -> CacheResult<()> {
  let file = OpenOptions::new()
    .create_new(true)
    .write(true)
    .open(marker)
    .map_err(|error| io_error("create restore marker", marker, error))?;
  file
    .sync_all()
    .map_err(|error| io_error("synchronize restore marker", marker, error))?;
  sync_directory(marker.parent().expect("restore marker has a parent"))
}

pub(super) fn read_journal(path: &Path) -> CacheResult<RestoreJournal> {
  let metadata = fs::symlink_metadata(path).map_err(|error| io_error("inspect restore journal", path, error))?;
  if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
    return Err(CacheError::Metadata(format!(
      "restore journal '{}' is not a regular file",
      path.display()
    )));
  }
  let size = metadata.len();
  if size > MAX_JOURNAL_BYTES {
    return Err(CacheError::Limit(format!(
      "restore journal '{}' exceeds {MAX_JOURNAL_BYTES} bytes",
      path.display()
    )));
  }
  let bytes = fs::read(path).map_err(|error| io_error("read restore journal", path, error))?;
  let journal = serde_json::from_slice(&bytes)
    .map_err(|error| CacheError::Metadata(format!("failed to decode '{}': {error}", path.display())))?;
  validate_journal(&journal)?;
  if path.file_stem().and_then(|value| value.to_str()) != Some(journal.id.as_str()) {
    return Err(CacheError::Metadata(format!(
      "restore journal '{}' is bound to a different transaction id",
      path.display()
    )));
  }
  Ok(journal)
}

pub(super) fn validate_journal(journal: &RestoreJournal) -> CacheResult<()> {
  if journal.version != JOURNAL_VERSION
    || journal.id.is_empty()
    || journal.id.len() > MAX_TRANSACTION_ID_BYTES
    || journal.roots.is_empty()
    || journal.roots.len() > MAX_CACHE_LIST_ITEMS
    || journal.created_parents.len() > MAX_CACHE_LIST_ITEMS
    || !journal.workspace.is_absolute()
  {
    return Err(CacheError::Metadata("invalid restore journal header".to_owned()));
  }
  let expected = transaction_path(&journal.workspace, &journal.id)?;
  if journal.transaction != expected {
    return Err(CacheError::Metadata(
      "restore journal transaction path is outside its workspace filesystem".to_owned(),
    ));
  }
  let roots = journal.roots.iter().map(|root| root.path.clone()).collect::<Vec<_>>();
  let canonical_roots = validate_output_roots(&roots)?;
  if roots != canonical_roots {
    return Err(CacheError::Metadata(
      "restore journal output roots are not in canonical order".to_owned(),
    ));
  }
  let mut previous_parent = None;
  for parent in &journal.created_parents {
    if parent.is_root()
      || previous_parent.is_some_and(|previous: &RelativePath| previous >= parent)
      || !roots.iter().any(|root| is_strict_ancestor(parent, root))
    {
      return Err(CacheError::Metadata(
        "restore journal contains an invalid created-parent set".to_owned(),
      ));
    }
    previous_parent = Some(parent);
  }
  Ok(())
}

fn is_strict_ancestor(parent: &RelativePath, path: &RelativePath) -> bool {
  path
    .as_str()
    .strip_prefix(parent.as_str())
    .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Removes all durable state after rollback or committed cleanup completes.
pub(super) fn remove_transaction_state(journal: &Path, transaction: &Path) -> CacheResult<()> {
  // Make the absence of `prepared` durable before deleting staging or backup.
  // A crash after transaction removal must never leave recovery believing it
  // still has enough state to roll back. A committed or rolled-back marker, if
  // present, remains visible throughout this transition and makes cleanup
  // idempotent; an unprepared transaction has never changed live outputs.
  remove_file_if_present(&prepared_marker(journal))?;
  sync_directory(journal.parent().expect("journal has a parent"))?;

  let removed_transaction = match fs::remove_dir_all(transaction) {
    Ok(()) => true,
    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
    Err(error) => return Err(io_error("remove restore transaction", transaction, error)),
  };
  if removed_transaction {
    sync_directory(transaction.parent().expect("a restore transaction has a parent"))?;
  }
  // Terminal markers are removed only after transaction deletion is durable.
  // If cleanup stops here, either terminal state safely requests another
  // cleanup pass; exposing a lone `prepared` marker is impossible.
  remove_file_if_present(&commit_marker(journal))?;
  remove_file_if_present(&rolled_back_marker(journal))?;
  remove_file_if_present(journal)?;
  sync_directory(journal.parent().expect("journal has a parent"))
}

pub(super) fn journal_files(root: &Path) -> CacheResult<Vec<PathBuf>> {
  let mut paths = Vec::new();
  for entry in fs::read_dir(root).map_err(|error| io_error("scan restore journals", root, error))? {
    let entry = entry.map_err(|error| io_error("read restore journal entry", root, error))?;
    let path = entry.path();
    if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
      if paths.len() >= MAX_CACHE_LIST_ITEMS {
        return Err(CacheError::Limit(format!(
          "restore recovery is limited to {MAX_CACHE_LIST_ITEMS} journals per pass"
        )));
      }
      let file_type = entry
        .file_type()
        .map_err(|error| io_error("inspect restore journal entry", &path, error))?;
      if !file_type.is_file() || file_type.is_symlink() {
        return Err(CacheError::Metadata(format!(
          "restore journal '{}' is not a regular file",
          path.display()
        )));
      }
      paths.push(path);
    }
  }
  paths.sort();
  Ok(paths)
}

/// Removes intent files abandoned before their atomic rename made them visible
/// to normal journal recovery. No transaction directory exists at that point.
pub(super) fn cleanup_temporary_journals(root: &Path) -> CacheResult<()> {
  for entry in fs::read_dir(root).map_err(|error| io_error("scan temporary restore journals", root, error))? {
    let path = entry
      .map_err(|error| io_error("read temporary restore journal entry", root, error))?
      .path();
    if path.extension().and_then(|extension| extension.to_str()) == Some("tmp") {
      remove_file_if_present(&path)?;
    }
  }
  Ok(())
}

pub(super) fn transaction_path(workspace: &Path, id: &str) -> CacheResult<PathBuf> {
  if id.is_empty()
    || id.len() > MAX_TRANSACTION_ID_BYTES
    || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
  {
    return Err(CacheError::Metadata("restore transaction id is invalid".to_owned()));
  }
  let parent = workspace
    .parent()
    .ok_or_else(|| CacheError::Configuration("cannot restore outputs in a filesystem root".to_owned()))?;
  Ok(parent.join(format!(".octa-restore-{id}")))
}

pub(super) fn commit_marker(journal: &Path) -> PathBuf {
  journal.with_extension("committed")
}

pub(super) fn prepared_marker(journal: &Path) -> PathBuf {
  journal.with_extension("prepared")
}

pub(super) fn rolled_back_marker(journal: &Path) -> PathBuf {
  journal.with_extension("rolled-back")
}

/// Checks a marker without following a cache-controlled symbolic link.
pub(super) fn marker_exists(marker: &Path) -> CacheResult<bool> {
  match fs::symlink_metadata(marker) {
    Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() && metadata.len() == 0 => {
      Ok(true)
    },
    Ok(_) => Err(CacheError::Metadata(format!(
      "restore marker '{}' is not an empty regular file",
      marker.display()
    ))),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(io_error("inspect restore marker", marker, error)),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn journal(workspace: &Path) -> RestoreJournal {
    let id = "valid-id".to_owned();
    RestoreJournal::new(
      id.clone(),
      workspace.to_path_buf(),
      transaction_path(workspace, &id).unwrap(),
      vec![JournalRoot {
        path: RelativePath::new("out").unwrap(),
        had_original: false,
      }],
      Vec::new(),
    )
  }

  #[test]
  fn rejects_invalid_headers_paths_and_output_sets() {
    let workspace = tempfile::tempdir().unwrap();
    let mut value = journal(workspace.path());
    value.version += 1;
    assert!(matches!(validate_journal(&value), Err(CacheError::Metadata(_))));

    let mut value = journal(workspace.path());
    value.transaction = workspace.path().join("outside");
    assert!(matches!(validate_journal(&value), Err(CacheError::Metadata(_))));

    let mut value = journal(workspace.path());
    value.roots.push(JournalRoot {
      path: RelativePath::new("out/nested").unwrap(),
      had_original: false,
    });
    assert!(matches!(validate_journal(&value), Err(CacheError::Configuration(_))));

    assert!(matches!(
      transaction_path(workspace.path(), "bad/id"),
      Err(CacheError::Metadata(_))
    ));

    let mut value = journal(workspace.path());
    value.created_parents = vec![RelativePath::new("unrelated").unwrap()];
    assert!(matches!(validate_journal(&value), Err(CacheError::Metadata(_))));

    let mut value = journal(workspace.path());
    value.roots = vec![
      JournalRoot {
        path: RelativePath::new("second").unwrap(),
        had_original: false,
      },
      JournalRoot {
        path: RelativePath::new("first").unwrap(),
        had_original: false,
      },
    ];
    assert!(matches!(validate_journal(&value), Err(CacheError::Metadata(_))));
  }

  #[test]
  fn reads_only_bounded_files_bound_to_their_transaction_name() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let value = journal(workspace.path());
    let wrong_name = root.path().join("wrong.json");
    fs::write(&wrong_name, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(matches!(read_journal(&wrong_name), Err(CacheError::Metadata(_))));

    let oversized = root.path().join("oversized.json");
    fs::File::create(&oversized)
      .unwrap()
      .set_len(MAX_JOURNAL_BYTES + 1)
      .unwrap();
    assert!(matches!(read_journal(&oversized), Err(CacheError::Limit(_))));

    let malformed_marker = root.path().join("marker.prepared");
    fs::write(&malformed_marker, b"not empty").unwrap();
    assert!(matches!(marker_exists(&malformed_marker), Err(CacheError::Metadata(_))));
    assert!(!marker_exists(&root.path().join("absent")).unwrap());
  }

  #[test]
  fn journal_scanning_and_absent_cleanup_are_idempotent() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("one.json"), b"invalid").unwrap();
    fs::write(root.path().join("ignored.tmp"), b"temporary").unwrap();
    assert_eq!(journal_files(root.path()).unwrap(), vec![root.path().join("one.json")]);
    cleanup_temporary_journals(root.path()).unwrap();
    assert!(!root.path().join("ignored.tmp").exists());
    assert!(matches!(
      read_journal(&root.path().join("one.json")),
      Err(CacheError::Metadata(_))
    ));
    fs::remove_file(root.path().join("one.json")).unwrap();
    remove_transaction_state(
      &root.path().join("absent.json"),
      &root.path().join("absent-transaction"),
    )
    .unwrap();
  }

  #[test]
  fn journal_scan_is_bounded_before_returning_an_untrusted_directory_listing() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..=MAX_CACHE_LIST_ITEMS {
      fs::write(root.path().join(format!("{index}.json")), []).unwrap();
    }
    assert!(matches!(journal_files(root.path()), Err(CacheError::Limit(_))));
  }

  #[test]
  fn cleanup_durably_clears_prepared_before_removing_transaction_state() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("transaction.json");
    let transaction = root.path().join("transaction-state");
    fs::write(&journal, []).unwrap();
    fs::write(prepared_marker(&journal), []).unwrap();
    fs::write(commit_marker(&journal), []).unwrap();
    // A regular file forces `remove_dir_all` to fail after the prepared marker
    // has been removed, exposing the ordering without a crash hook.
    fs::write(&transaction, []).unwrap();

    assert!(remove_transaction_state(&journal, &transaction).is_err());
    assert!(!prepared_marker(&journal).exists());
    assert!(commit_marker(&journal).exists());
    assert!(journal.exists());
  }

  #[cfg(unix)]
  #[test]
  fn filesystem_root_cannot_host_a_sibling_transaction() {
    assert!(matches!(
      transaction_path(Path::new("/"), "valid"),
      Err(CacheError::Configuration(_))
    ));
  }
}
