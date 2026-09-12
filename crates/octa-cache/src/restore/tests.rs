//! Transactional restore and crash-recovery tests.

use std::{fs, io, panic::AssertUnwindSafe};

use octa_cache_protocol::RelativePath;

use super::*;
use crate::{pack_bundle, BundleEncoding};

fn roots() -> Vec<RelativePath> {
  vec![
    RelativePath::new("artifacts").unwrap(),
    RelativePath::new("reports").unwrap(),
  ]
}

fn write_tree(workspace: &Path, label: &str) {
  fs::create_dir_all(workspace.join("artifacts")).unwrap();
  fs::create_dir_all(workspace.join("reports")).unwrap();
  fs::write(workspace.join("artifacts/result.bin"), format!("{label}-artifact")).unwrap();
  fs::write(workspace.join("reports/junit.xml"), format!("{label}-report")).unwrap();
}

fn assert_tree(workspace: &Path, label: &str) {
  assert_eq!(
    fs::read_to_string(workspace.join("artifacts/result.bin")).unwrap(),
    format!("{label}-artifact")
  );
  assert_eq!(
    fs::read_to_string(workspace.join("reports/junit.xml")).unwrap(),
    format!("{label}-report")
  );
}

fn packed_tree() -> (Vec<u8>, BlobDescriptor) {
  let producer = tempfile::tempdir().unwrap();
  write_tree(producer.path(), "new");
  let packed = pack_bundle(
    Vec::new(),
    producer.path(),
    &roots(),
    BundleEncoding::ZstdV1 { level: 1 },
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap();
  (packed.writer, packed.descriptor)
}

fn restore_manager(state: &Path) -> RestoreManager {
  RestoreManager::open(state, BundleLimits::default()).unwrap()
}

#[test]
fn restores_outputs_and_skips_an_already_materialized_tree() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let (bytes, descriptor) = packed_tree();
  let manager = restore_manager(state.path());

  assert_eq!(
    manager
      .restore(
        &bytes[..],
        &descriptor,
        workspace.path(),
        &roots(),
        &CancellationToken::new()
      )
      .unwrap(),
    RestoreOutcome::Restored
  );
  assert_tree(workspace.path(), "new");

  struct Unreadable;
  impl Read for Unreadable {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
      panic!("an already materialized restore must not read its bundle")
    }
  }
  assert_eq!(
    manager
      .restore(
        Unreadable,
        &descriptor,
        workspace.path(),
        &roots(),
        &CancellationToken::new()
      )
      .unwrap(),
    RestoreOutcome::AlreadyMaterialized
  );
}

#[test]
fn validation_failure_never_replaces_live_outputs() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let (bytes, descriptor) = packed_tree();
  let manager = restore_manager(state.path());

  let error = manager
    .restore_validated(
      &bytes[..],
      &descriptor,
      workspace.path(),
      &roots(),
      &CancellationToken::new(),
      |staging| {
        assert_tree(staging, "new");
        Err(CacheError::Metadata("rejected staged metadata".to_owned()))
      },
    )
    .unwrap_err();

  assert!(matches!(error, CacheError::Metadata(message) if message == "rejected staged metadata"));
  assert_tree(workspace.path(), "old");
  assert!(journal_files(&manager.journal_root()).unwrap().is_empty());
}

#[test]
fn every_durable_restore_stage_recovers_to_a_complete_generation() {
  // Two existing roots produce: intent, prepared, backup/install per root, commit.
  for fail_at in 0..7 {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    write_tree(workspace.path(), "old");
    let (bytes, descriptor) = packed_tree();
    let manager = restore_manager(state.path());
    let mut stage = 0;
    let crashed = std::panic::catch_unwind(AssertUnwindSafe(|| {
      let _ = manager.restore_with_observer(
        &bytes[..],
        &descriptor,
        workspace.path(),
        &roots(),
        &CancellationToken::new(),
        |_| {
          if stage == fail_at {
            panic!("injected crash after durable stage {fail_at}");
          }
          stage += 1;
        },
      );
    }));
    assert!(crashed.is_err());

    let restarted = restore_manager(state.path());
    assert_eq!(restarted.recover().unwrap(), 1);
    if fail_at == 6 {
      assert_tree(workspace.path(), "new");
    } else {
      assert_tree(workspace.path(), "old");
    }
    assert!(journal_files(&restarted.journal_root()).unwrap().is_empty());
  }
}

#[test]
fn cancellation_during_replacement_rolls_back_before_returning() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let (bytes, descriptor) = packed_tree();
  let cancel = CancellationToken::new();
  let manager = restore_manager(state.path());
  let result = manager.restore_with_observer(&bytes[..], &descriptor, workspace.path(), &roots(), &cancel, |stage| {
    if stage == "staged_output_installed" {
      cancel.cancel();
    }
  });
  assert!(matches!(result, Err(CacheError::Cancelled)));
  assert_tree(workspace.path(), "old");
  assert!(journal_files(&manager.journal_root()).unwrap().is_empty());
}

#[test]
fn cancellation_during_materialized_inspection_is_not_treated_as_a_miss() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let (_, descriptor) = packed_tree();
  let cancel = CancellationToken::new();
  cancel.cancel();
  let error = restore_manager(state.path())
    .restore(io::empty(), &descriptor, workspace.path(), &roots(), &cancel)
    .unwrap_err();
  assert!(matches!(error, CacheError::Cancelled));
  assert_tree(workspace.path(), "old");
}

#[test]
fn recovery_removes_new_outputs_and_parents_when_no_original_existed() {
  let producer = tempfile::tempdir().unwrap();
  fs::create_dir_all(producer.path().join("generated/api")).unwrap();
  fs::write(producer.path().join("generated/api/client.rs"), "generated").unwrap();
  let nested_roots = vec![RelativePath::new("generated/api").unwrap()];
  let packed = pack_bundle(
    Vec::new(),
    producer.path(),
    &nested_roots,
    BundleEncoding::Identity,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap();
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());
  let mut installed = false;
  let crashed = std::panic::catch_unwind(AssertUnwindSafe(|| {
    let _ = manager.restore_with_observer(
      &packed.writer[..],
      &packed.descriptor,
      workspace.path(),
      &nested_roots,
      &CancellationToken::new(),
      |stage| {
        if stage == "staged_output_installed" {
          installed = true;
          panic!("injected crash after installing a previously absent output");
        }
      },
    );
  }));
  assert!(crashed.is_err());
  assert!(installed);
  assert!(workspace.path().join("generated/api/client.rs").exists());

  assert_eq!(manager.recover().unwrap(), 1);
  assert!(!workspace.path().join("generated").exists());
}

#[test]
fn output_locking_rejects_empty_duplicate_and_overlapping_roots() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());
  assert!(matches!(
    manager.lock_outputs(workspace.path(), &[]),
    Err(CacheError::Configuration(_))
  ));
  let duplicate = RelativePath::new("out").unwrap();
  assert!(matches!(
    manager.lock_outputs(workspace.path(), &[duplicate.clone(), duplicate.clone()]),
    Err(CacheError::Configuration(_))
  ));
  assert!(matches!(
    manager.lock_outputs(workspace.path(), &[duplicate, RelativePath::new("out/nested").unwrap()]),
    Err(CacheError::Configuration(_))
  ));
  manager
    .lock_outputs(workspace.path(), &[RelativePath::new("out").unwrap()])
    .unwrap();

  // Recovery helpers are deliberately idempotent: an ancestor created by a
  // concurrent preparatory step is acceptable, while the portable root maps
  // to the workspace itself rather than to an empty child name.
  fs::create_dir(workspace.path().join("existing")).unwrap();
  create_live_parents(workspace.path(), &[RelativePath::new("existing").unwrap()]).unwrap();
  assert_eq!(join_relative(workspace.path(), &RelativePath::root()), workspace.path());
}

#[test]
fn malformed_bundle_and_unsafe_live_ancestor_leave_workspace_untouched() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let (_, descriptor) = packed_tree();
  let manager = restore_manager(state.path());
  assert!(manager
    .restore(
      &b"invalid"[..],
      &descriptor,
      workspace.path(),
      &roots(),
      &CancellationToken::new()
    )
    .is_err());
  assert_tree(workspace.path(), "old");
  assert!(journal_files(&manager.journal_root()).unwrap().is_empty());

  let producer = tempfile::tempdir().unwrap();
  fs::create_dir_all(producer.path().join("generated/api")).unwrap();
  fs::write(producer.path().join("generated/api/result"), "new").unwrap();
  let nested = vec![RelativePath::new("generated/api").unwrap()];
  let packed = pack_bundle(
    Vec::new(),
    producer.path(),
    &nested,
    BundleEncoding::Identity,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap();
  fs::write(workspace.path().join("generated"), "not a directory").unwrap();
  assert!(matches!(
    manager.restore(
      &packed.writer[..],
      &packed.descriptor,
      workspace.path(),
      &nested,
      &CancellationToken::new()
    ),
    Err(CacheError::Path { .. })
  ));
  assert_eq!(
    fs::read_to_string(workspace.path().join("generated")).unwrap(),
    "not a directory"
  );
}

#[test]
fn recovery_discards_a_transaction_for_a_deleted_workspace() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let workspace_path = workspace.path().to_path_buf();
  let (bytes, descriptor) = packed_tree();
  let manager = restore_manager(state.path());
  let crashed = std::panic::catch_unwind(AssertUnwindSafe(|| {
    let _ = manager.restore_with_observer(
      &bytes[..],
      &descriptor,
      &workspace_path,
      &roots(),
      &CancellationToken::new(),
      |stage| {
        if stage == "journal_prepared" {
          panic!("leave a prepared journal");
        }
      },
    );
  }));
  assert!(crashed.is_err());
  fs::remove_dir_all(&workspace_path).unwrap();
  assert_eq!(manager.recover().unwrap(), 1);
  assert!(journal_files(&manager.journal_root()).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn recovery_rejects_a_workspace_root_replaced_by_a_symlink() {
  use std::os::unix::fs::symlink;

  let state = tempfile::tempdir().unwrap();
  let parent = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  let workspace = parent.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  fs::write(outside.path().join("sentinel"), "unchanged").unwrap();
  let manager = restore_manager(state.path());
  let id = "redirected-workspace".to_owned();
  let canonical_workspace = dunce::canonicalize(&workspace).unwrap();
  let transaction = transaction_path(&canonical_workspace, &id).unwrap();
  let journal = RestoreJournal::new(
    id.clone(),
    canonical_workspace,
    transaction,
    vec![JournalRoot {
      path: RelativePath::new("out").unwrap(),
      had_original: false,
    }],
    Vec::new(),
  );
  let journal_path = manager.journal_root().join(format!("{id}.json"));
  write_journal(&journal_path, &journal).unwrap();
  fs::remove_dir(&workspace).unwrap();
  symlink(outside.path(), &workspace).unwrap();

  assert!(matches!(manager.recover(), Err(CacheError::Path { .. })));
  assert_eq!(
    fs::read_to_string(outside.path().join("sentinel")).unwrap(),
    "unchanged"
  );
  assert!(journal_path.exists());
}

#[test]
fn committed_recovery_refuses_to_hide_a_missing_live_output() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let (bytes, descriptor) = packed_tree();
  let manager = restore_manager(state.path());
  let crashed = std::panic::catch_unwind(AssertUnwindSafe(|| {
    let _ = manager.restore_with_observer(
      &bytes[..],
      &descriptor,
      workspace.path(),
      &roots(),
      &CancellationToken::new(),
      |stage| {
        if stage == "transaction_committed" {
          panic!("leave a committed journal");
        }
      },
    );
  }));
  assert!(crashed.is_err());
  fs::remove_dir_all(workspace.path().join("reports")).unwrap();
  assert!(matches!(manager.recover(), Err(CacheError::Metadata(_))));
}

#[test]
fn outputs_match_propagates_mid_operation_cancellation() {
  let workspace = tempfile::tempdir().unwrap();
  write_tree(workspace.path(), "old");
  let (_, descriptor) = packed_tree();
  let cancel = CancellationToken::new();
  cancel.cancel();
  assert!(matches!(
    outputs_match(
      workspace.path(),
      &roots(),
      &descriptor,
      BundleLimits::default(),
      &cancel
    ),
    Err(CacheError::Cancelled)
  ));
}

#[test]
fn rollback_fails_closed_when_durable_state_is_ambiguous() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());

  let first_id = "lost-original".to_owned();
  let first_transaction = transaction_path(workspace.path(), &first_id).unwrap();
  fs::create_dir_all(first_transaction.join("staging")).unwrap();
  fs::create_dir_all(first_transaction.join("backup")).unwrap();
  let first = RestoreJournal::new(
    first_id,
    workspace.path().to_path_buf(),
    first_transaction,
    vec![JournalRoot {
      path: RelativePath::new("out").unwrap(),
      had_original: true,
    }],
    Vec::new(),
  );
  let first_journal = manager.journal_root().join("lost-original.json");
  assert!(matches!(
    manager.rollback(&first_journal, &first),
    Err(CacheError::Metadata(message)) if message.contains("lost both staged and backup")
  ));

  let second_id = "unexpected-live".to_owned();
  let second_transaction = transaction_path(workspace.path(), &second_id).unwrap();
  fs::create_dir_all(second_transaction.join("staging/out")).unwrap();
  fs::create_dir_all(second_transaction.join("backup")).unwrap();
  fs::create_dir_all(workspace.path().join("out")).unwrap();
  let second = RestoreJournal::new(
    second_id,
    workspace.path().to_path_buf(),
    second_transaction,
    vec![JournalRoot {
      path: RelativePath::new("out").unwrap(),
      had_original: false,
    }],
    Vec::new(),
  );
  let second_journal = manager.journal_root().join("unexpected-live.json");
  assert!(matches!(
    manager.rollback(&second_journal, &second),
    Err(CacheError::Metadata(message)) if message.contains("unexpected live output")
  ));

  let third_id = "staged-and-live".to_owned();
  let third_transaction = transaction_path(workspace.path(), &third_id).unwrap();
  fs::create_dir_all(third_transaction.join("staging/ambiguous")).unwrap();
  fs::create_dir_all(third_transaction.join("backup/ambiguous")).unwrap();
  fs::create_dir_all(workspace.path().join("ambiguous")).unwrap();
  let third = RestoreJournal::new(
    third_id,
    workspace.path().to_path_buf(),
    third_transaction,
    vec![JournalRoot {
      path: RelativePath::new("ambiguous").unwrap(),
      had_original: true,
    }],
    Vec::new(),
  );
  assert!(matches!(
    manager.rollback(&manager.journal_root().join("staged-and-live.json"), &third),
    Err(CacheError::Metadata(message)) if message.contains("both staged and live")
  ));

  let fourth_id = "lost-new-output".to_owned();
  let fourth_transaction = transaction_path(workspace.path(), &fourth_id).unwrap();
  fs::create_dir_all(fourth_transaction.join("staging")).unwrap();
  fs::create_dir_all(fourth_transaction.join("backup")).unwrap();
  let fourth = RestoreJournal::new(
    fourth_id,
    workspace.path().to_path_buf(),
    fourth_transaction,
    vec![JournalRoot {
      path: RelativePath::new("missing-new-output").unwrap(),
      had_original: false,
    }],
    Vec::new(),
  );
  assert!(matches!(
    manager.rollback(&manager.journal_root().join("lost-new-output.json"), &fourth),
    Err(CacheError::Metadata(message)) if message.contains("lost both staged and live")
  ));
}

#[test]
fn recovery_repeats_a_partially_completed_rollback_safely() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());
  let id = "partial-rollback".to_owned();
  let canonical_workspace = dunce::canonicalize(workspace.path()).unwrap();
  let transaction = transaction_path(&canonical_workspace, &id).unwrap();
  fs::create_dir_all(transaction.join("staging")).unwrap();
  fs::create_dir_all(transaction.join("backup")).unwrap();
  fs::write(transaction.join("staging/out"), "new").unwrap();
  // This is the durable state after rollback already moved the installed
  // output back to staging and restored the original, but before recording its
  // terminal marker.
  fs::write(workspace.path().join("out"), "old").unwrap();
  let journal = RestoreJournal::new(
    id.clone(),
    canonical_workspace,
    transaction.clone(),
    vec![JournalRoot {
      path: RelativePath::new("out").unwrap(),
      had_original: true,
    }],
    Vec::new(),
  );
  let journal_path = manager.journal_root().join(format!("{id}.json"));
  write_journal(&journal_path, &journal).unwrap();
  write_prepared_marker(&journal_path).unwrap();

  assert_eq!(manager.recover().unwrap(), 1);
  assert_eq!(fs::read_to_string(workspace.path().join("out")).unwrap(), "old");
  assert!(!transaction.exists());
  assert!(!journal_path.exists());
}

#[test]
fn rolled_back_marker_makes_recovery_cleanup_only() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());
  let id = "rollback-complete".to_owned();
  let canonical_workspace = dunce::canonicalize(workspace.path()).unwrap();
  let transaction = transaction_path(&canonical_workspace, &id).unwrap();
  fs::create_dir_all(transaction.join("staging")).unwrap();
  fs::write(workspace.path().join("out"), "old").unwrap();
  let journal = RestoreJournal::new(
    id.clone(),
    canonical_workspace,
    transaction.clone(),
    vec![JournalRoot {
      path: RelativePath::new("out").unwrap(),
      had_original: true,
    }],
    Vec::new(),
  );
  let journal_path = manager.journal_root().join(format!("{id}.json"));
  write_journal(&journal_path, &journal).unwrap();
  write_prepared_marker(&journal_path).unwrap();
  write_rolled_back_marker(&journal_path).unwrap();

  assert_eq!(manager.recover().unwrap(), 1);
  assert_eq!(fs::read_to_string(workspace.path().join("out")).unwrap(), "old");
  assert!(!transaction.exists());
  assert!(!journal_path.exists());
}

#[test]
fn recovery_rejects_conflicting_terminal_markers() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());
  let id = "conflicting-terminal-markers".to_owned();
  let canonical_workspace = dunce::canonicalize(workspace.path()).unwrap();
  let transaction = transaction_path(&canonical_workspace, &id).unwrap();
  let journal = RestoreJournal::new(
    id.clone(),
    canonical_workspace,
    transaction,
    vec![JournalRoot {
      path: RelativePath::new("out").unwrap(),
      had_original: false,
    }],
    Vec::new(),
  );
  let journal_path = manager.journal_root().join(format!("{id}.json"));
  write_journal(&journal_path, &journal).unwrap();
  write_commit_marker(&journal_path).unwrap();
  write_rolled_back_marker(&journal_path).unwrap();

  assert!(manager
    .recover()
    .unwrap_err()
    .to_string()
    .contains("conflicting terminal markers"));
}

#[test]
fn recovery_rejects_a_noncanonical_workspace_path() {
  let workspace = tempfile::tempdir().unwrap();
  let noncanonical = workspace.path().join(".");
  assert!(matches!(
    recovery_workspace_exists(&noncanonical),
    Err(CacheError::Path { reason, .. }) if reason.contains("canonical journal path")
  ));
}

#[test]
fn rename_durability_syncs_distinct_parent_directories() {
  let directory = tempfile::tempdir().unwrap();
  let source_parent = directory.path().join("source");
  let destination_parent = directory.path().join("destination");
  fs::create_dir(&source_parent).unwrap();
  fs::create_dir(&destination_parent).unwrap();

  sync_rename_parents(&source_parent.join("entry"), &destination_parent.join("entry")).unwrap();
}

#[test]
fn markerless_cleanup_journal_never_changes_live_outputs() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());
  let id = "cleanup-interrupted".to_owned();
  let canonical_workspace = dunce::canonicalize(workspace.path()).unwrap();
  let transaction = transaction_path(&canonical_workspace, &id).unwrap();
  fs::write(workspace.path().join("out"), "complete generation").unwrap();
  let journal = RestoreJournal::new(
    id.clone(),
    canonical_workspace,
    transaction,
    vec![JournalRoot {
      path: RelativePath::new("out").unwrap(),
      had_original: true,
    }],
    Vec::new(),
  );
  let journal_path = manager.journal_root().join(format!("{id}.json"));
  write_journal(&journal_path, &journal).unwrap();

  assert_eq!(manager.recover().unwrap(), 1);
  assert_eq!(
    fs::read_to_string(workspace.path().join("out")).unwrap(),
    "complete generation"
  );
  assert!(!journal_path.exists());
}

#[test]
fn rollback_removes_a_new_file_output_and_keeps_nonempty_parents() {
  let state = tempfile::tempdir().unwrap();
  let workspace = tempfile::tempdir().unwrap();
  let manager = restore_manager(state.path());
  let id = "new-file".to_owned();
  let transaction = transaction_path(workspace.path(), &id).unwrap();
  fs::create_dir_all(transaction.join("staging")).unwrap();
  fs::create_dir_all(transaction.join("backup")).unwrap();
  fs::create_dir_all(workspace.path().join("generated")).unwrap();
  fs::write(workspace.path().join("generated/keep.txt"), "keep").unwrap();
  fs::write(workspace.path().join("generated/result.txt"), "new").unwrap();
  let journal = RestoreJournal::new(
    id,
    workspace.path().to_path_buf(),
    transaction,
    vec![JournalRoot {
      path: RelativePath::new("generated/result.txt").unwrap(),
      had_original: false,
    }],
    vec![RelativePath::new("generated").unwrap()],
  );
  let journal_path = manager.journal_root().join("new-file.json");
  manager.rollback(&journal_path, &journal).unwrap();
  assert!(!workspace.path().join("generated/result.txt").exists());
  assert_eq!(
    fs::read_to_string(workspace.path().join("generated/keep.txt")).unwrap(),
    "keep"
  );
}

#[test]
fn restore_rejects_a_missing_workspace_before_creating_state() {
  let state = tempfile::tempdir().unwrap();
  let missing_workspace = state.path().join("missing-workspace");
  let (_, descriptor) = packed_tree();
  let manager = restore_manager(state.path());
  assert!(matches!(
    manager.restore(
      io::empty(),
      &descriptor,
      &missing_workspace,
      &roots(),
      &CancellationToken::new()
    ),
    Err(CacheError::Io { .. })
  ));
  assert!(journal_files(&manager.journal_root()).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn filesystem_permission_failures_leave_restore_state_explicit() {
  use std::os::unix::fs::PermissionsExt as _;

  let workspace = tempfile::tempdir().unwrap();
  let protected = workspace.path().join("protected");
  fs::create_dir(&protected).unwrap();
  fs::write(protected.join("output"), "data").unwrap();
  fs::set_permissions(&protected, fs::Permissions::from_mode(0o000)).unwrap();

  let mut missing = BTreeSet::new();
  assert!(matches!(
    validate_live_ancestors(
      workspace.path(),
      &RelativePath::new("protected/nested/output").unwrap(),
      &mut missing
    ),
    Err(CacheError::Io { .. })
  ));
  assert!(matches!(
    path_exists(&protected.join("output")),
    Err(CacheError::Io { .. })
  ));
  assert!(matches!(
    remove_file_if_present(&protected.join("output")),
    Err(CacheError::Io { .. })
  ));
  assert!(matches!(sync_tree_directories(&protected), Err(CacheError::Io { .. })));

  fs::set_permissions(&protected, fs::Permissions::from_mode(0o500)).unwrap();
  assert!(matches!(
    create_live_parents(workspace.path(), &[RelativePath::new("protected/new").unwrap()]),
    Err(CacheError::Io { .. })
  ));
  fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
}
