//! Input snapshot integration and filesystem-safety tests.

use std::fs;

use tempfile::TempDir;

use super::*;

fn patterns(values: &[&str]) -> Vec<String> {
  values.iter().map(|value| (*value).to_owned()).collect()
}

#[tokio::test]
async fn snapshots_a_portable_tree_and_ignores_excluded_files() {
  let first = TempDir::new().unwrap();
  let second = TempDir::new().unwrap();
  for root in [first.path(), second.path()] {
    fs::create_dir_all(root.join("src/empty")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("src/debug.log"), "ignored\n").unwrap();
    fs::write(root.join("src/generated.rs"), "excluded\n").unwrap();
    fs::write(root.join(".octaignore"), "*.log\n").unwrap();
  }
  let inputs = patterns(&["src", "!src/generated.rs"]);
  let snapshotter = InputSnapshotter::default();
  let left = snapshotter
    .snapshot(first.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  let right = snapshotter
    .snapshot(second.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();

  assert_eq!(left, right);
  assert_eq!(
    left.root.to_string(),
    "blake3:31b55def498c0ba8e6f838626e7746ab8ad709d1932f132f354ebd358a66f946:110"
  );
  assert_eq!(left.entries.len(), 3);
  assert!(matches!(&left.entries[0], InputEntry::Directory { path } if path.as_str() == "src"));
  assert!(matches!(&left.entries[1], InputEntry::Directory { path } if path.as_str() == "src/empty"));
  assert!(
    matches!(&left.entries[2], InputEntry::File { path, content, .. } if path.as_str() == "src/main.rs" && *content == Digest::blake3(b"fn main() {}\n"))
  );
}

#[tokio::test]
async fn preserves_ordered_patterns_and_nested_ignore_semantics() {
  let root = TempDir::new().unwrap();
  fs::create_dir_all(root.path().join("src/generated")).unwrap();
  fs::write(root.path().join("src/main.rs"), "main").unwrap();
  fs::write(root.path().join("src/generated/drop.rs"), "drop").unwrap();
  fs::write(root.path().join("src/generated/keep.rs"), "keep").unwrap();
  fs::write(root.path().join(".octaignore"), "*.rs\n").unwrap();
  fs::write(root.path().join("src/.octaignore"), "!main.rs\n!generated/\n").unwrap();
  fs::write(root.path().join("src/generated/.octaignore"), "!keep.rs\n").unwrap();

  let snapshot = InputSnapshotter::default()
    .snapshot(
      root.path(),
      &patterns(&["src", "!src/generated/drop.rs"]),
      &CancellationToken::new(),
    )
    .await
    .unwrap();
  let paths = snapshot
    .entries
    .iter()
    .map(|entry| entry.path().as_str())
    .collect::<Vec<_>>();
  assert_eq!(
    paths,
    [
      "src",
      "src/.octaignore",
      "src/generated",
      "src/generated/.octaignore",
      "src/generated/keep.rs",
      "src/main.rs"
    ]
  );

  fs::write(root.path().join("!important"), "literal").unwrap();
  let literal = InputSnapshotter::default()
    .snapshot(root.path(), &patterns(&[r"\!important"]), &CancellationToken::new())
    .await
    .unwrap();
  assert_eq!(literal.entries[0].path().as_str(), "!important");
}

#[tokio::test]
async fn content_path_mode_and_link_target_affect_the_root() {
  let root = TempDir::new().unwrap();
  fs::create_dir(root.path().join("src")).unwrap();
  fs::write(root.path().join("src/input"), "one").unwrap();
  let snapshotter = InputSnapshotter::default();
  let inputs = patterns(&["src"]);
  let first = snapshotter
    .snapshot(root.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  fs::write(root.path().join("src/input"), "two").unwrap();
  let second = snapshotter
    .snapshot(root.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  assert_ne!(first.root, second.root);

  #[cfg(unix)]
  {
    use std::os::unix::fs::{symlink, PermissionsExt as _};
    let mut permissions = fs::metadata(root.path().join("src/input")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(root.path().join("src/input"), permissions).unwrap();
    let executable = snapshotter
      .snapshot(root.path(), &inputs, &CancellationToken::new())
      .await
      .unwrap();
    assert_ne!(second.root, executable.root);

    symlink("input", root.path().join("src/link")).unwrap();
    let linked = snapshotter
      .snapshot(root.path(), &inputs, &CancellationToken::new())
      .await
      .unwrap();
    assert!(matches!(linked.entries.last(), Some(InputEntry::Symlink { target, .. }) if target == "input"));
    assert_ne!(executable.root, linked.root);
  }
}

#[tokio::test]
async fn rejects_unsafe_inputs_and_honours_cancellation() {
  let root = TempDir::new().unwrap();
  fs::write(root.path().join("input"), "data").unwrap();
  let snapshotter = InputSnapshotter::default();
  for input in ["", "../outside", "/absolute", r"bad\path"] {
    let error = snapshotter
      .snapshot(root.path(), &patterns(&[input]), &CancellationToken::new())
      .await
      .unwrap_err();
    assert!(matches!(error, CacheError::Configuration(_)));
  }

  let cancel = CancellationToken::new();
  cancel.cancel();
  assert!(matches!(
    snapshotter.snapshot(root.path(), &patterns(&["input"]), &cancel).await,
    Err(CacheError::Cancelled)
  ));

  assert!(InputSnapshotter::new(SnapshotOptions {
    max_parallel_hashes: 0,
    ..SnapshotOptions::default()
  })
  .is_err());
  assert!(InputSnapshotter::new(SnapshotOptions {
    max_entries: 0,
    ..SnapshotOptions::default()
  })
  .is_err());
  assert!(InputSnapshotter::new(SnapshotOptions {
    read_buffer_bytes: 1,
    ..SnapshotOptions::default()
  })
  .is_err());

  assert!(InputSnapshotter::default()
    .snapshot(root.path(), &patterns(&["["]), &CancellationToken::new())
    .await
    .is_err());
  assert!(InputSnapshotter::default()
    .snapshot(&root.path().join("missing"), &[], &CancellationToken::new())
    .await
    .is_err());

  let bounded = InputSnapshotter::new(SnapshotOptions {
    max_entries: 1,
    ..SnapshotOptions::default()
  })
  .unwrap();
  assert!(matches!(
    bounded
      .snapshot(root.path(), &patterns(&["**"]), &CancellationToken::new())
      .await,
    Err(CacheError::Limit(_))
  ));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_special_files_and_workspace_escaping_symlinks() {
  use std::os::unix::{fs::symlink, net::UnixListener};

  let root = TempDir::new().unwrap();
  UnixListener::bind(root.path().join("socket")).unwrap();
  let snapshotter = InputSnapshotter::default();
  assert!(matches!(
    snapshotter
      .snapshot(root.path(), &patterns(&["socket"]), &CancellationToken::new())
      .await,
    Err(CacheError::UnsupportedEntry { .. })
  ));

  symlink("../outside", root.path().join("escape")).unwrap();
  assert!(matches!(
    snapshotter
      .snapshot(root.path(), &patterns(&["escape"]), &CancellationToken::new())
      .await,
    Err(CacheError::Path { .. })
  ));

  let external = TempDir::new().unwrap();
  fs::write(external.path().join("target"), "outside").unwrap();
  symlink(external.path(), root.path().join("external-dir")).unwrap();
  symlink("external-dir/target", root.path().join("indirect-escape")).unwrap();
  assert!(matches!(
    snapshotter
      .snapshot(root.path(), &patterns(&["indirect-escape"]), &CancellationToken::new())
      .await,
    Err(CacheError::Path { .. })
  ));
}

#[tokio::test]
async fn empty_inputs_have_a_stable_digest_across_snapshotter_clones() {
  let snapshotter = InputSnapshotter::new(SnapshotOptions {
    max_parallel_hashes: 2,
    ..SnapshotOptions::default()
  })
  .unwrap();
  let clone = snapshotter.clone();

  let root = TempDir::new().unwrap();
  let first = snapshotter
    .snapshot(root.path(), &[], &CancellationToken::new())
    .await
    .unwrap();
  let second = clone
    .snapshot(root.path(), &[], &CancellationToken::new())
    .await
    .unwrap();
  assert!(first.entries.is_empty());
  assert_eq!(first.root, second.root);
}

#[tokio::test]
async fn sequential_hardlinks_reuse_the_snapshot_digest() {
  use std::time::Duration;

  let root = TempDir::new().unwrap();
  let first = root.path().join("first");
  let second = root.path().join("second");
  fs::write(&first, "shared data").unwrap();
  fs::hard_link(&first, &second).unwrap();
  let snapshotter = InputSnapshotter::new(SnapshotOptions {
    max_parallel_hashes: 1,
    ..SnapshotOptions::default()
  })
  .unwrap();
  let memo = Mutex::new(HashMap::new());

  let first_hash = snapshotter
    .hash_file(
      &first,
      fs::symlink_metadata(&first).unwrap(),
      &memo,
      &CancellationToken::new(),
    )
    .await
    .unwrap();
  let held = snapshotter.scheduler.hold_permit().await;
  let second_hash = tokio::time::timeout(
    Duration::from_secs(1),
    snapshotter.hash_file(
      &second,
      fs::symlink_metadata(&second).unwrap(),
      &memo,
      &CancellationToken::new(),
    ),
  )
  .await
  .expect("a memoized hardlink must not wait for hashing capacity")
  .unwrap();
  drop(held);

  assert_eq!(first_hash.content, second_hash.content);
}

#[test]
fn sha256_file_identity_changes_the_input_tree_digest() {
  let sha_entry = InputEntry::File {
    path: RelativePath::new("input").unwrap(),
    content: Digest::new(DigestAlgorithm::Sha256, [7; 32], 9),
    executable: false,
  };
  assert_ne!(
    input_root_digest(std::slice::from_ref(&sha_entry)),
    input_root_digest(&[])
  );
  assert_eq!(sha_entry.path().as_str(), "input");
}

#[test]
fn portable_path_validation_rejects_every_escape_form() {
  let workspace = TempDir::new().unwrap();
  fs::create_dir(workspace.path().join("nested")).unwrap();
  fs::write(workspace.path().join("target"), "target").unwrap();
  // macOS exposes temporary directories through `/var`, whose canonical path
  // starts with `/private/var`. Production snapshots always use a canonical
  // workspace, so keep this low-level test under the same invariant.
  let workspace = dunce::canonicalize(workspace.path()).unwrap();
  let link = workspace.join("nested/link");

  assert_eq!(
    safe_symlink_target(&workspace, &link, Path::new("../target")).unwrap(),
    "../target"
  );
  assert_eq!(
    safe_symlink_target(&workspace, &link, Path::new(".././target")).unwrap(),
    ".././target"
  );
  assert!(safe_symlink_target(&workspace, &link, Path::new("/absolute")).is_err());
  assert!(safe_symlink_target(&workspace, Path::new("/outside/link"), Path::new("target")).is_err());
  for target in [r"bad\target", "C:target", "bad\nname", "../../../escape"] {
    assert!(safe_symlink_target(&workspace, &link, Path::new(target)).is_err());
  }
  assert!(portable_relative(&workspace, Path::new("/outside/input")).is_err());

  #[cfg(unix)]
  {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
    let invalid_name = OsString::from_vec(vec![0xff]);
    let invalid = workspace.join(&invalid_name);
    assert!(portable_relative(&workspace, &invalid).is_err());
    assert!(safe_symlink_target(&workspace, &link, &std::path::PathBuf::from(invalid_name)).is_err());
  }
}
