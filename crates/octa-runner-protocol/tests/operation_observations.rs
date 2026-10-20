use octa_runner_protocol::{
  is_public_operation_path, FileChangeKind, ObservedOperation, OperationObservation, OperationPhase,
};

fn command() -> OperationObservation {
  OperationObservation {
    operation_id: "0123456789abcdef".repeat(4),
    phase: OperationPhase::Completed,
    observed_at_unix_ms: 0,
    operation: ObservedOperation::Command {
      exit_code: Some(17),
      failed: Some(true),
    },
  }
}

#[test]
fn public_paths_are_bounded_portable_and_exclude_protected_content() {
  for path in ["src/main.rs", "out/new file.txt", &"a".repeat(1024)] {
    assert!(is_public_operation_path(path), "{path:?}");
  }
  for path in [
    "",
    "/tmp/file",
    "file/",
    "src//file",
    "../file",
    "src/./file",
    ".env",
    "src/.hidden/file",
    "protected/file",
    "src/protected/file",
    "C:/file",
    "src\\file",
    "line\nfile",
    "null\0file",
    &"a".repeat(1025),
  ] {
    assert!(!is_public_operation_path(path), "{path:?}");
  }
}

#[test]
fn observations_require_an_opaque_identity_and_nonnegative_time() {
  let original = command();
  assert!(original.is_valid());
  for id in ["", &"a".repeat(63), &"a".repeat(65), &"A".repeat(64), &"g".repeat(64)] {
    let mut observation = original.clone();
    observation.operation_id = id.to_owned();
    assert!(!observation.is_valid(), "{id:?}");
  }
  let mut observation = original;
  observation.observed_at_unix_ms = -1;
  assert!(!observation.is_valid());
}

#[test]
fn command_start_cannot_claim_an_outcome_and_unknown_completion_remains_valid() {
  for (phase, exit_code, failed, valid) in [
    (OperationPhase::Started, None, None, true),
    (OperationPhase::Started, Some(0), None, false),
    (OperationPhase::Started, None, Some(false), false),
    (OperationPhase::Started, Some(17), Some(true), false),
    (OperationPhase::Completed, None, None, true),
    (OperationPhase::Completed, Some(0), None, true),
    (OperationPhase::Completed, None, Some(false), true),
    (OperationPhase::Completed, Some(17), Some(true), true),
  ] {
    let mut observation = command();
    observation.phase = phase;
    observation.operation = ObservedOperation::Command { exit_code, failed };
    assert_eq!(observation.is_valid(), valid, "{observation:?}");
  }
}

#[test]
fn file_observations_require_public_paths_and_only_complete_with_outcomes() {
  for change in [
    FileChangeKind::Created,
    FileChangeKind::Modified,
    FileChangeKind::Deleted,
  ] {
    let mut observation = command();
    observation.operation = ObservedOperation::File {
      path: "src/main.rs".to_owned(),
      change,
      failed: None,
    };
    observation.phase = OperationPhase::Started;
    assert!(observation.is_valid());
    if let ObservedOperation::File { failed, .. } = &mut observation.operation {
      *failed = Some(false);
    }
    assert!(!observation.is_valid());
    observation.phase = OperationPhase::Completed;
    assert!(observation.is_valid());
    if let ObservedOperation::File { path, .. } = &mut observation.operation {
      *path = "protected/test.rs".to_owned();
    }
    assert!(!observation.is_valid());
  }
}
