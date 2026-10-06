use std::{fs, path::Path};

use octa_plugin::protocol::PluginResponse;
use serde_json::{json, Value};

use cap_std::{ambient_authority, fs::Dir};

use super::{
  config::Deliverable,
  records::RunRecordPaths,
  resources::{self, ResourceKind, ResourceSet},
};

fn deliverable(value: Value) -> Deliverable {
  serde_json::from_value(value).expect("resource test deliverable must be statically valid")
}

fn record_paths(workspace: &Path) -> RunRecordPaths {
  let directory = workspace.join(".octa/codex-runs/run-fixture");
  fs::create_dir_all(&directory).unwrap();
  for name in ["trace.jsonl", "result.json", "provenance.json"] {
    fs::write(directory.join(name), b"{}\n").unwrap();
  }
  RunRecordPaths {
    trace: ".octa/codex-runs/run-fixture/trace.jsonl".to_owned(),
    result: ".octa/codex-runs/run-fixture/result.json".to_owned(),
    provenance: ".octa/codex-runs/run-fixture/provenance.json".to_owned(),
  }
}

#[test]
fn validates_every_resource_before_building_standard_declarations() {
  let workspace = tempfile::tempdir().unwrap();
  let records = record_paths(workspace.path());
  fs::create_dir_all(workspace.path().join("out/bundle")).unwrap();
  fs::write(workspace.path().join("out/bundle/item"), b"item").unwrap();
  fs::write(workspace.path().join("out/report.json"), b"{}\n").unwrap();
  let deliverables = vec![
    deliverable(json!({
      "kind": "artifact",
      "name": "bundle",
      "path": "out/bundle",
      "content_type": "application/octet-stream"
    })),
    deliverable(json!({
      "kind": "report",
      "name": "review",
      "path": "out/report.json",
      "format": "codex.review.v1"
    })),
  ];

  let resources = ResourceSet::inspect(workspace.path(), &records, &deliverables).unwrap();
  resources.ensure_unchanged().unwrap();
  let responses = resources.responses("command");

  assert_eq!(responses.len(), 5);
  assert!(matches!(
    &responses[0],
    PluginResponse::RegisterArtifact { artifact, .. }
      if artifact.name == "codex-run-trace"
        && artifact.path == Path::new(".octa/codex-runs/run-fixture/trace.jsonl")
  ));
  assert!(matches!(
    &responses[2],
    PluginResponse::RegisterReport { report, .. }
      if report.name == "codex-run-result" && report.format == "octa.codex.result.v1"
  ));
  assert!(matches!(
    &responses[3],
    PluginResponse::RegisterArtifact { artifact, .. } if artifact.name == "bundle"
  ));
  assert!(matches!(
    &responses[4],
    PluginResponse::RegisterReport { report, .. } if report.name == "review"
  ));
}

#[test]
fn missing_resources_produce_no_declaration_set() {
  let workspace = tempfile::tempdir().unwrap();
  let records = record_paths(workspace.path());
  fs::write(workspace.path().join("present"), b"present").unwrap();

  let missing = vec![
    deliverable(json!({ "kind": "artifact", "name": "present", "path": "present" })),
    deliverable(json!({ "kind": "artifact", "name": "missing", "path": "missing" })),
  ];
  let error = ResourceSet::inspect(workspace.path(), &records, &missing)
    .err()
    .expect("a missing required resource must fail the whole set");
  assert!(error.to_string().contains("missing or inaccessible"));
}

#[test]
fn an_in_place_mutation_between_validation_passes_is_rejected() {
  let workspace = tempfile::tempdir().unwrap();
  let records = record_paths(workspace.path());
  let path = workspace.path().join("deliverable");
  fs::write(&path, b"first").unwrap();
  let deliverables = vec![deliverable(
    json!({ "kind": "artifact", "name": "output", "path": "deliverable" }),
  )];
  let resources = ResourceSet::inspect(workspace.path(), &records, &deliverables).unwrap();

  fs::write(path, b"a different generation").unwrap();

  let error = resources
    .ensure_unchanged()
    .expect_err("a resource changed during validation must not be declared");
  assert!(error.to_string().contains("changed during final validation"));
}

#[cfg(unix)]
#[test]
fn symlink_escapes_and_special_files_are_rejected() {
  use std::{ffi::CString, os::unix::ffi::OsStrExt as _, os::unix::fs::symlink};

  let workspace = tempfile::tempdir().unwrap();
  let outside = tempfile::NamedTempFile::new().unwrap();
  let outside_directory = tempfile::tempdir().unwrap();
  fs::write(outside_directory.path().join("file"), b"outside").unwrap();
  let records = record_paths(workspace.path());
  symlink(outside.path(), workspace.path().join("escape")).unwrap();
  let escaped = vec![deliverable(
    json!({ "kind": "artifact", "name": "escape", "path": "escape" }),
  )];
  let error = ResourceSet::inspect(workspace.path(), &records, &escaped)
    .err()
    .expect("an escaping symlink must fail");
  assert!(error.to_string().contains("symbolic link"));

  symlink(outside_directory.path(), workspace.path().join("linked-directory")).unwrap();
  let ancestor_escape = vec![deliverable(json!({
    "kind": "artifact",
    "name": "ancestor-escape",
    "path": "linked-directory/file"
  }))];
  let error = ResourceSet::inspect(workspace.path(), &records, &ancestor_escape)
    .err()
    .expect("an escaping ancestor symlink must fail");
  assert!(error.to_string().contains("symbolic link"));

  let pipe = workspace.path().join("device");
  let pipe = CString::new(pipe.as_os_str().as_bytes()).unwrap();
  // SAFETY: `pipe` is a NUL-terminated pathname and the mode contains only
  // ordinary FIFO permission bits.
  assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
  let special = vec![deliverable(
    json!({ "kind": "artifact", "name": "device", "path": "device" }),
  )];
  let error = ResourceSet::inspect(workspace.path(), &records, &special)
    .err()
    .expect("a special filesystem node must fail");
  assert!(error.to_string().contains("regular file or directory"));
}

#[cfg(unix)]
#[test]
fn replacing_a_validated_file_with_a_symlink_fails_revalidation() {
  use std::os::unix::fs::symlink;

  let workspace = tempfile::tempdir().unwrap();
  let records = record_paths(workspace.path());
  let path = workspace.path().join("deliverable");
  fs::write(&path, b"first").unwrap();
  let deliverables = vec![deliverable(
    json!({ "kind": "artifact", "name": "output", "path": "deliverable" }),
  )];
  let resources = ResourceSet::inspect(workspace.path(), &records, &deliverables).unwrap();
  let outside = tempfile::NamedTempFile::new().unwrap();

  fs::remove_file(&path).unwrap();
  symlink(outside.path(), path).unwrap();

  let error = resources
    .ensure_unchanged()
    .expect_err("a symlink introduced during validation must not be declared");
  assert!(error.to_string().contains("symbolic link"));
}

#[test]
fn report_directories_are_rejected() {
  let workspace = tempfile::tempdir().unwrap();
  let records = record_paths(workspace.path());
  fs::create_dir(workspace.path().join("report-dir")).unwrap();
  let directory_report = vec![deliverable(json!({
    "kind": "report",
    "name": "review",
    "path": "report-dir",
    "format": "review.v1"
  }))];
  let error = ResourceSet::inspect(workspace.path(), &records, &directory_report)
    .err()
    .expect("a report directory must fail");
  assert!(error.to_string().contains("must be a regular file"));
}

#[test]
fn internal_open_boundaries_fail_closed_on_impossible_or_changed_types() {
  let workspace = tempfile::tempdir().unwrap();
  let file_path = workspace.path().join("file");
  fs::write(&file_path, b"file").unwrap();
  let root = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();

  let Err(error) = resources::open_resource(
    &root,
    "file/child",
    // This value does not affect ancestor validation.
    resources::ExpectedKind::FileOrDirectory,
  ) else {
    panic!("a file cannot serve as a resource ancestor");
  };
  assert!(error.to_string().contains("non-directory ancestor"));

  let Err(error) = resources::open_resource(&root, "", resources::ExpectedKind::File) else {
    panic!("an empty internal path must fail");
  };
  assert!(error.to_string().contains("path must not be empty"));

  let Err(error) = resources::open_directory(&root, "file", "file") else {
    panic!("a file cannot be reopened as a directory");
  };
  assert!(error.to_string().contains("could not be opened safely"));

  let Err(error) = resources::OpenedResource::from_file(fs::File::open(&file_path).unwrap(), ResourceKind::Directory)
  else {
    panic!("an opened file cannot satisfy a directory snapshot");
  };
  assert!(error.to_string().contains("changed type"));
}

#[cfg(unix)]
#[test]
fn opened_metadata_still_recognizes_a_symbolic_link() {
  use std::os::unix::fs::symlink;

  let workspace = tempfile::tempdir().unwrap();
  fs::write(workspace.path().join("target"), b"target").unwrap();
  let link = workspace.path().join("link");
  symlink("target", &link).unwrap();

  assert!(crate::filesystem::is_std_link_or_reparse(
    &fs::symlink_metadata(link).unwrap()
  ));
}
