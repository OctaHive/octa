//! Input snapshot integration and filesystem-safety tests.

use std::{fs, sync::Arc};

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
async fn independent_contracts_cannot_exclude_each_others_required_inputs() {
  let root = TempDir::new().unwrap();
  fs::create_dir_all(root.path().join("src/generated")).unwrap();
  fs::write(root.path().join("src/main.rs"), "main").unwrap();
  fs::write(root.path().join("src/generated/schema.rs"), "schema").unwrap();
  let pattern_sets = vec![
    patterns(&["src/**", "!src/generated/**"]),
    patterns(&["src/generated/schema.rs"]),
  ];

  let snapshot = InputSnapshotter::default()
    .snapshot_pattern_sets(root.path(), &pattern_sets, &CancellationToken::new())
    .await
    .unwrap();
  let paths = snapshot
    .entries
    .iter()
    .map(|entry| entry.path().as_str())
    .collect::<Vec<_>>();

  assert!(paths.contains(&"src/main.rs"));
  assert!(paths.contains(&"src/generated/schema.rs"));
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
  assert!(InputSnapshotter::new(SnapshotOptions {
    max_memo_bytes: 0,
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
  assert_eq!(format!("{first:?}"), format!("{second:?}"));
}

#[tokio::test]
async fn hardlinks_share_one_content_identity_in_a_snapshot() {
  let root = TempDir::new().unwrap();
  let first = root.path().join("first");
  let second = root.path().join("second");
  fs::write(&first, "shared data").unwrap();
  fs::hard_link(&first, &second).unwrap();
  let snapshot = InputSnapshotter::default()
    .snapshot(root.path(), &patterns(&["first", "second"]), &CancellationToken::new())
    .await
    .unwrap();
  let digests = snapshot
    .entries
    .iter()
    .map(|entry| match entry {
      InputEntry::File { content, .. } => *content,
      _ => panic!("hardlink inputs must be files"),
    })
    .collect::<Vec<_>>();
  assert_eq!(digests, [Digest::blake3(b"shared data"); 2]);

  let workspace = dunce::canonicalize(root.path()).unwrap();
  let files = [workspace.join("first"), workspace.join("second")]
    .into_iter()
    .enumerate()
    .map(|(validation_index, path)| {
      let key = EntryKey::new(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();
      FileInput {
        validation_index,
        path,
        key,
      }
    })
    .collect::<Vec<_>>();
  let (requests, indices, counts) = unique_hash_requests(&files);
  assert_eq!(requests.len(), 1, "one hardlinked inode must produce one content read");
  assert_eq!(indices, [0, 0]);
  assert_eq!(counts, [2]);
}

#[tokio::test]
async fn unchanged_snapshot_revalidation_accepts_the_captured_metadata() {
  let root = TempDir::new().unwrap();
  fs::create_dir(root.path().join("src")).unwrap();
  fs::write(root.path().join("src/input"), "stable content").unwrap();
  let inputs = patterns(&["src/**"]);
  let snapshotter = InputSnapshotter::new(SnapshotOptions {
    max_parallel_hashes: 1,
    ..SnapshotOptions::default()
  })
  .unwrap();
  let snapshot = snapshotter
    .snapshot(root.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();

  let unchanged = snapshotter
    .revalidate(root.path(), &[inputs], &snapshot, &CancellationToken::new())
    .await
    .unwrap();

  assert!(unchanged);
}

#[tokio::test]
async fn snapshot_revalidation_detects_content_and_membership_changes() {
  let root = TempDir::new().unwrap();
  fs::create_dir(root.path().join("src")).unwrap();
  let input = root.path().join("src/input");
  fs::write(&input, "before").unwrap();
  let inputs = patterns(&["src/**"]);
  let snapshotter = InputSnapshotter::default();

  let before_content = snapshotter
    .snapshot(root.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  fs::write(&input, "after!").unwrap();
  assert!(!snapshotter
    .revalidate(
      root.path(),
      std::slice::from_ref(&inputs),
      &before_content,
      &CancellationToken::new(),
    )
    .await
    .unwrap());

  let before_membership = snapshotter
    .snapshot(root.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  fs::write(root.path().join("src/added"), "new").unwrap();
  assert!(!snapshotter
    .revalidate(
      root.path(),
      std::slice::from_ref(&inputs),
      &before_membership,
      &CancellationToken::new(),
    )
    .await
    .unwrap());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn persistent_digest_memo_reuses_only_unchanged_input_identity() {
  let workspace = TempDir::new().unwrap();
  let cache = TempDir::new().unwrap();
  let input = workspace.path().join("input");
  fs::write(&input, "before").unwrap();
  let store = Arc::new(crate::LocalCacheStore::open(crate::LocalCacheConfig::new(cache.path())).unwrap());
  let inputs = patterns(&["input"]);

  let first = InputSnapshotter::default()
    .with_digest_memo(store.clone())
    .snapshot(workspace.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  // A separate snapshotter models the next Octa invocation and proves that
  // reuse comes from the durable local CAS rather than process-local state.
  let unchanged = InputSnapshotter::default()
    .with_digest_memo(store.clone())
    .snapshot(workspace.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  assert_eq!(unchanged, first);

  // Keep the byte length constant: size alone must never validate a memo.
  fs::write(&input, "after!").unwrap();
  let changed = InputSnapshotter::default()
    .with_digest_memo(store)
    .snapshot(workspace.path(), &inputs, &CancellationToken::new())
    .await
    .unwrap();
  assert_ne!(changed.root, first.root);
}

#[test]
fn memo_validation_rejects_non_blake3_file_identities() {
  let workspace = TempDir::new().unwrap();
  fs::write(workspace.path().join("input"), "contents").unwrap();
  let workspace = dunce::canonicalize(workspace.path()).unwrap();
  let path = workspace.join("input");
  let validation = vec![validation_entry(&workspace, &path, fs::symlink_metadata(&path).unwrap()).unwrap()];
  let invalid_entries = vec![InputEntry::File {
    path: RelativePath::new("input").unwrap(),
    content: Digest::new(DigestAlgorithm::Sha256, [7; 32], 8),
    executable: false,
  }];
  let invalid = crate::digest_memo::MemoSnapshot {
    root: input_root_digest(&invalid_entries),
    entries: invalid_entries,
  };

  assert!(!memo_matches(&invalid, &validation));
}

#[test]
fn persistent_memo_identity_is_bound_to_its_boot_and_filesystem_scope() {
  let workspace = TempDir::new().unwrap();
  fs::write(workspace.path().join("input"), "contents").unwrap();
  let workspace = dunce::canonicalize(workspace.path()).unwrap();
  let path = workspace.join("input");
  let validation = vec![validation_entry(&workspace, &path, fs::symlink_metadata(&path).unwrap()).unwrap()];

  assert!(input_metadata_digest(&validation, None).is_none());
  assert_ne!(
    input_metadata_digest(&validation, Some(b"boot-and-filesystem-a")),
    input_metadata_digest(&validation, Some(b"boot-and-filesystem-b"))
  );
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
