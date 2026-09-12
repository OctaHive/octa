//! Bundle round-trip, canonical-format, limit, and path-safety tests.

use std::{fs, io::Cursor, path::Path};

use octa_cache_protocol::{BlobEncoding, RelativePath};
use proptest::prelude::*;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::*;

fn root(value: &str) -> RelativePath {
  RelativePath::new(value).unwrap()
}

fn pack(workspace: &Path, roots: &[RelativePath], encoding: BundleEncoding) -> PackedBundle<Vec<u8>> {
  pack_bundle(
    Vec::new(),
    workspace,
    roots,
    encoding,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap()
}

#[test]
fn round_trips_deterministically_with_both_encodings() {
  let workspace = TempDir::new().unwrap();
  fs::create_dir_all(workspace.path().join("out/empty")).unwrap();
  fs::write(workspace.path().join("out/a.txt"), "alpha").unwrap();
  fs::write(workspace.path().join("out/b.txt"), "beta").unwrap();
  let roots = vec![root("out")];

  let identity = pack(workspace.path(), &roots, BundleEncoding::Identity);
  let identity_again = pack(workspace.path(), &roots, BundleEncoding::Identity);
  let compressed = pack(workspace.path(), &roots, BundleEncoding::ZstdV1 { level: 1 });
  assert_eq!(identity.writer, identity_again.writer);
  assert_eq!(
    identity.descriptor.digest.to_string(),
    "blake3:eba1855e80c84ed8767f4612ca90be47d791fd7cc6f8f1496f242efaca6eecd8:85"
  );
  assert_eq!(identity.descriptor.digest, compressed.descriptor.digest);
  assert_eq!(
    identity.descriptor.expanded_size_bytes,
    compressed.descriptor.expanded_size_bytes
  );

  for packed in [identity, identity_again] {
    let staging = TempDir::new().unwrap();
    extract_bundle(
      Cursor::new(packed.writer),
      &packed.descriptor,
      staging.path(),
      &roots,
      BundleLimits::default(),
      &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(staging.path().join("out/a.txt")).unwrap(), "alpha");
    assert!(staging.path().join("out/empty").is_dir());
  }

  let staging = TempDir::new().unwrap();
  extract_bundle(
    Cursor::new(compressed.writer),
    &compressed.descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap();
  assert_eq!(fs::read_to_string(staging.path().join("out/b.txt")).unwrap(), "beta");
}

#[test]
fn canonical_tree_order_keeps_directory_subtrees_contiguous() {
  let workspace = TempDir::new().unwrap();
  fs::create_dir_all(workspace.path().join("out/a")).unwrap();
  fs::write(workspace.path().join("out/a/file"), "nested").unwrap();
  fs::write(workspace.path().join("out/a-file"), "sibling").unwrap();
  let roots = [root("out")];

  let packed = pack(workspace.path(), &roots, BundleEncoding::Identity);
  let staging = TempDir::new().unwrap();
  extract_bundle(
    Cursor::new(packed.writer),
    &packed.descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap();

  assert_eq!(fs::read_to_string(staging.path().join("out/a/file")).unwrap(), "nested");
  assert_eq!(
    fs::read_to_string(staging.path().join("out/a-file")).unwrap(),
    "sibling"
  );
}

#[test]
fn rejects_an_encoded_stream_shorter_than_its_descriptor() {
  let workspace = TempDir::new().unwrap();
  fs::write(workspace.path().join("out"), "data").unwrap();
  let roots = vec![root("out")];

  for encoding in [BundleEncoding::Identity, BundleEncoding::ZstdV1 { level: 1 }] {
    let mut packed = pack(workspace.path(), &roots, encoding);
    packed.descriptor.encoded_size_bytes += 1;
    let staging = TempDir::new().unwrap();
    assert!(extract_bundle(
      Cursor::new(packed.writer),
      &packed.descriptor,
      staging.path(),
      &roots,
      BundleLimits::default(),
      &CancellationToken::new(),
    )
    .is_err());
  }
}

#[cfg(unix)]
#[test]
fn preserves_executable_files_and_safe_symlinks() {
  use std::os::unix::fs::{symlink, PermissionsExt as _};

  let workspace = TempDir::new().unwrap();
  fs::create_dir(workspace.path().join("out")).unwrap();
  let executable = workspace.path().join("out/tool");
  fs::write(&executable, "#!/bin/sh\n").unwrap();
  let mut permissions = fs::metadata(&executable).unwrap().permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&executable, permissions).unwrap();
  symlink("tool", workspace.path().join("out/latest")).unwrap();

  let roots = vec![root("out")];
  let packed = pack(workspace.path(), &roots, BundleEncoding::ZstdV1 { level: 1 });
  assert!(BundleLimits {
    max_encoded_bytes: packed.descriptor.encoded_size_bytes - 1,
    ..BundleLimits::default()
  }
  .validate_descriptor(&packed.descriptor)
  .is_err());
  let staging = TempDir::new().unwrap();
  extract_bundle(
    Cursor::new(&packed.writer),
    &packed.descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap();
  assert_ne!(
    fs::metadata(staging.path().join("out/tool"))
      .unwrap()
      .permissions()
      .mode()
      & 0o111,
    0
  );
  assert_eq!(
    fs::read_link(staging.path().join("out/latest")).unwrap(),
    Path::new("tool")
  );
}

#[test]
fn validates_configuration_limits_and_staging() {
  let workspace = TempDir::new().unwrap();
  fs::write(workspace.path().join("out"), "data").unwrap();
  for roots in [
    Vec::new(),
    vec![RelativePath::root()],
    vec![root("out"), root("out")],
    vec![root("out"), root("out/nested")],
    vec![root("a"), root("a-b"), root("a/nested")],
  ] {
    assert!(pack_bundle(
      Vec::new(),
      workspace.path(),
      &roots,
      BundleEncoding::Identity,
      BundleLimits::default(),
      &CancellationToken::new(),
    )
    .is_err());
  }

  let invalid_limits = BundleLimits {
    max_entries: 0,
    ..BundleLimits::default()
  };
  assert!(pack_bundle(
    Vec::new(),
    workspace.path(),
    &[root("out")],
    BundleEncoding::Identity,
    invalid_limits,
    &CancellationToken::new(),
  )
  .is_err());

  let invalid_limits = BundleLimits {
    read_buffer_bytes: 1,
    ..BundleLimits::default()
  };
  assert!(pack_bundle(
    Vec::new(),
    workspace.path(),
    &[root("out")],
    BundleEncoding::Identity,
    invalid_limits,
    &CancellationToken::new(),
  )
  .is_err());

  assert!(pack_bundle(
    Vec::new(),
    workspace.path(),
    &[root("out")],
    BundleEncoding::ZstdV1 { level: 23 },
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());

  let packed = pack(workspace.path(), &[root("out")], BundleEncoding::Identity);
  let staging = TempDir::new().unwrap();
  fs::write(staging.path().join("occupied"), "x").unwrap();
  assert!(extract_bundle(
    Cursor::new(&packed.writer),
    &packed.descriptor,
    staging.path(),
    &[root("out")],
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());

  let staging_file = workspace.path().join("staging-file");
  fs::write(&staging_file, "x").unwrap();
  assert!(extract_bundle(
    Cursor::new(&packed.writer),
    &packed.descriptor,
    &staging_file,
    &[root("out")],
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());
  assert!(extract_bundle(
    Cursor::new(&packed.writer),
    &packed.descriptor,
    Path::new("relative-staging"),
    &[root("out")],
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());
}

#[test]
fn extraction_requires_every_declared_output_root() {
  let bytes = one_directory_bundle("first");
  let descriptor = identity_descriptor(&bytes, 1);
  let staging = TempDir::new().unwrap();

  assert!(matches!(
    extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      staging.path(),
      &[root("first"), root("second")],
      BundleLimits::default(),
      &CancellationToken::new(),
    ),
    Err(CacheError::InvalidBundle(message)) if message.contains("second")
  ));
}

#[test]
fn extraction_rejects_a_path_instead_of_the_next_declared_root() {
  let mut entries = directory_entry("first");
  entries.extend(directory_entry("unexpected"));
  let bytes = raw_bundle(&entries);
  let descriptor = identity_descriptor(&bytes, 2);
  let staging = TempDir::new().unwrap();

  assert!(matches!(
    extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      staging.path(),
      &[root("first"), root("second")],
      BundleLimits::default(),
      &CancellationToken::new(),
    ),
    Err(CacheError::InvalidBundle(message)) if message.contains("second")
  ));
}

#[test]
fn multiple_declared_roots_round_trip_in_canonical_order() {
  let workspace = TempDir::new().unwrap();
  fs::create_dir_all(workspace.path().join("nested/first")).unwrap();
  fs::write(workspace.path().join("second"), "value").unwrap();
  let roots = [root("second"), root("nested/first")];
  let packed = pack(workspace.path(), &roots, BundleEncoding::Identity);
  let staging = TempDir::new().unwrap();

  extract_bundle(
    Cursor::new(packed.writer),
    &packed.descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .unwrap();

  assert!(staging.path().join("nested/first").is_dir());
  assert_eq!(fs::read_to_string(staging.path().join("second")).unwrap(), "value");
}

#[test]
fn extraction_requires_an_explicit_immediate_parent_directory() {
  let mut entries = directory_entry("out");
  entries.extend(file_entry("out/missing/file", false, b"data"));
  let bytes = raw_bundle(&entries);
  let descriptor = identity_descriptor(&bytes, 2);
  let staging = TempDir::new().unwrap();

  assert!(matches!(
    extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      staging.path(),
      &[root("out")],
      BundleLimits::default(),
      &CancellationToken::new(),
    ),
    Err(CacheError::InvalidBundle(message)) if message.contains("explicit parent")
  ));
}

#[test]
fn enforces_pack_and_descriptor_resource_limits() {
  let workspace = TempDir::new().unwrap();
  fs::create_dir(workspace.path().join("out")).unwrap();
  fs::write(workspace.path().join("out/file"), vec![0_u8; 10_000]).unwrap();
  let roots = [root("out")];
  for limits in [
    BundleLimits {
      max_entries: 1,
      ..BundleLimits::default()
    },
    BundleLimits {
      max_path_bytes: 2,
      ..BundleLimits::default()
    },
    BundleLimits {
      max_file_bytes: 4,
      ..BundleLimits::default()
    },
    BundleLimits {
      max_expanded_bytes: 4,
      ..BundleLimits::default()
    },
    BundleLimits {
      max_encoded_bytes: 4,
      ..BundleLimits::default()
    },
  ] {
    assert!(pack_bundle(
      Vec::new(),
      workspace.path(),
      &roots,
      BundleEncoding::Identity,
      limits,
      &CancellationToken::new(),
    )
    .is_err());
  }

  let packed = pack(workspace.path(), &roots, BundleEncoding::ZstdV1 { level: 1 });
  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(&packed.writer),
    &packed.descriptor,
    staging.path(),
    &roots,
    BundleLimits {
      max_encoded_bytes: packed.descriptor.encoded_size_bytes - 1,
      ..BundleLimits::default()
    },
    &CancellationToken::new(),
  )
  .is_err());

  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(&packed.writer),
    &packed.descriptor,
    staging.path(),
    &roots,
    BundleLimits {
      max_compression_ratio: 1,
      ..BundleLimits::default()
    },
    &CancellationToken::new(),
  )
  .is_err());
}

#[test]
fn pack_entry_budget_includes_siblings_waiting_above_a_nested_directory() {
  let workspace = TempDir::new().unwrap();
  fs::create_dir_all(workspace.path().join("out/a")).unwrap();
  fs::write(workspace.path().join("out/a/one"), "one").unwrap();
  fs::write(workspace.path().join("out/a/two"), "two").unwrap();
  fs::write(workspace.path().join("out/b"), "sibling").unwrap();

  assert!(matches!(
    pack_bundle(
      Vec::new(),
      workspace.path(),
      &[root("out")],
      BundleEncoding::Identity,
      BundleLimits {
        // out, out/a, out/b, and at most one child below out/a.
        max_entries: 4,
        ..BundleLimits::default()
      },
      &CancellationToken::new(),
    ),
    Err(CacheError::Limit(_))
  ));
}

#[test]
fn rejects_corruption_truncation_wrong_roots_and_unsafe_headers() {
  let workspace = TempDir::new().unwrap();
  fs::write(workspace.path().join("out"), "data").unwrap();
  let roots = vec![root("out")];
  let packed = pack(workspace.path(), &roots, BundleEncoding::Identity);

  let mut corrupted = packed.writer.clone();
  *corrupted.last_mut().unwrap() ^= 1;
  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(corrupted),
    &packed.descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());

  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(&packed.writer[..packed.writer.len() - 1]),
    &packed.descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());

  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(packed.writer),
    &packed.descriptor,
    staging.path(),
    &[root("different")],
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());

  for path in ["../escape", ".", "/absolute"] {
    let bytes = one_directory_bundle(path);
    let descriptor = identity_descriptor(&bytes, 1);
    let staging = TempDir::new().unwrap();
    assert!(extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      staging.path(),
      &[root("out")],
      BundleLimits::default(),
      &CancellationToken::new(),
    )
    .is_err());
  }

  let mut trailing = one_directory_bundle("out");
  trailing.push(0);
  let descriptor = identity_descriptor(&trailing, 1);
  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(trailing),
    &descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());
}

#[test]
fn rejects_malformed_entry_types_ordering_and_structure() {
  let roots = [root("out")];
  let cases = [
    (1, raw_bundle(&[9, 0, 0, 0, 3, b'o', b'u', b't'])),
    (
      1,
      raw_bundle(&[FILE_TAG, 0, 0, 0, 3, b'o', b'u', b't', 2, 0, 0, 0, 0, 0, 0, 0, 0]),
    ),
    (
      1,
      raw_bundle(&[SYMLINK_TAG, 0, 0, 0, 3, b'o', b'u', b't', 0, 0, 0, 0, 0]),
    ),
    (1, raw_bundle(&[DIRECTORY_TAG, 0, 0, 0, 1, 0xff])),
    {
      let mut entries = directory_entry("out");
      entries.extend(directory_entry("out/z"));
      entries.extend(directory_entry("out/a"));
      (3, raw_bundle(&entries))
    },
    {
      let mut entries = directory_entry("out");
      entries.extend(symlink_entry("out/link", "target"));
      entries.extend(directory_entry("out/link/child"));
      (3, raw_bundle(&entries))
    },
  ];
  for (entry_count, bytes) in cases {
    let descriptor = identity_descriptor(&bytes, entry_count);
    let staging = TempDir::new().unwrap();
    assert!(extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      staging.path(),
      &roots,
      BundleLimits::default(),
      &CancellationToken::new(),
    )
    .is_err());
  }

  let mut wrong_magic = one_directory_bundle("out");
  wrong_magic[0] = b'X';
  let descriptor = identity_descriptor(&wrong_magic, 1);
  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(wrong_magic),
    &descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());

  let bytes = one_directory_bundle("out");
  let descriptor = identity_descriptor(&bytes, 2);
  let staging = TempDir::new().unwrap();
  assert!(extract_bundle(
    Cursor::new(bytes),
    &descriptor,
    staging.path(),
    &roots,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());
}

#[test]
fn rejects_files_larger_than_the_extraction_limit_and_cancellation() {
  let bytes = raw_bundle(&file_entry("out", false, b"12345"));
  let descriptor = identity_descriptor(&bytes, 1);
  let roots = [root("out")];
  let staging = TempDir::new().unwrap();
  assert!(matches!(
    extract_bundle(
      Cursor::new(&bytes),
      &descriptor,
      staging.path(),
      &roots,
      BundleLimits {
        max_file_bytes: 4,
        ..BundleLimits::default()
      },
      &CancellationToken::new(),
    ),
    Err(CacheError::Limit(_))
  ));

  let staging = TempDir::new().unwrap();
  let cancel = CancellationToken::new();
  cancel.cancel();
  assert!(matches!(
    extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      staging.path(),
      &roots,
      BundleLimits::default(),
      &cancel,
    ),
    Err(CacheError::Cancelled)
  ));
}

#[test]
fn validates_internal_portable_path_boundaries() {
  assert!(portable_relative(Path::new("/workspace"), Path::new("/outside/file")).is_err());
  assert!(validate_symlink_text(&root("out/link"), "../target").is_ok());
  assert!(validate_symlink_text(&root("out/link"), "./target").is_ok());
  for target in [
    "",
    "/absolute",
    r"bad\target",
    "C:target",
    "bad\nname",
    "../../../escape",
  ] {
    assert!(validate_symlink_text(&root("out/link"), target).is_err());
  }
  assert!(prepare_entry_parent(Path::new("/"), true).is_err());

  #[cfg(unix)]
  {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
    let invalid = Path::new("/workspace").join(OsString::from_vec(vec![0xff]));
    assert!(portable_relative(Path::new("/workspace"), &invalid).is_err());

    use std::os::unix::fs::symlink;
    let staging_parent = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let staging = staging_parent.path().join("staging");
    symlink(target.path(), &staging).unwrap();
    assert!(check_empty_staging(&staging).is_err());
  }
}

#[cfg(unix)]
#[test]
fn rejects_special_outputs_and_indirect_symlink_escapes() {
  use std::os::unix::{fs::symlink, net::UnixListener};

  let workspace = TempDir::new().unwrap();
  UnixListener::bind(workspace.path().join("socket")).unwrap();
  assert!(pack_bundle(
    Vec::new(),
    workspace.path(),
    &[root("socket")],
    BundleEncoding::Identity,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());

  let external = TempDir::new().unwrap();
  fs::write(external.path().join("target"), "outside").unwrap();
  symlink(external.path(), workspace.path().join("external-dir")).unwrap();
  symlink("external-dir/target", workspace.path().join("escape")).unwrap();
  assert!(pack_bundle(
    Vec::new(),
    workspace.path(),
    &[root("escape")],
    BundleEncoding::Identity,
    BundleLimits::default(),
    &CancellationToken::new(),
  )
  .is_err());
}

#[test]
fn cancellation_never_reports_a_complete_bundle() {
  let workspace = TempDir::new().unwrap();
  fs::write(workspace.path().join("out"), "data").unwrap();
  let cancel = CancellationToken::new();
  cancel.cancel();
  assert!(matches!(
    pack_bundle(
      Vec::new(),
      workspace.path(),
      &[root("out")],
      BundleEncoding::Identity,
      BundleLimits::default(),
      &cancel,
    ),
    Err(CacheError::Cancelled)
  ));
}

fn one_directory_bundle(path: &str) -> Vec<u8> {
  let mut bytes = Vec::new();
  bytes.extend_from_slice(BUNDLE_MAGIC);
  bytes.push(DIRECTORY_TAG);
  bytes.extend_from_slice(&(path.len() as u32).to_be_bytes());
  bytes.extend_from_slice(path.as_bytes());
  bytes
}

fn raw_bundle(body: &[u8]) -> Vec<u8> {
  let mut bytes = Vec::new();
  bytes.extend_from_slice(BUNDLE_MAGIC);
  bytes.extend_from_slice(body);
  bytes
}

fn directory_entry(path: &str) -> Vec<u8> {
  let mut bytes = vec![DIRECTORY_TAG];
  bytes.extend_from_slice(&(path.len() as u32).to_be_bytes());
  bytes.extend_from_slice(path.as_bytes());
  bytes
}

fn symlink_entry(path: &str, target: &str) -> Vec<u8> {
  let mut bytes = vec![SYMLINK_TAG];
  bytes.extend_from_slice(&(path.len() as u32).to_be_bytes());
  bytes.extend_from_slice(path.as_bytes());
  bytes.push(0);
  bytes.extend_from_slice(&(target.len() as u32).to_be_bytes());
  bytes.extend_from_slice(target.as_bytes());
  bytes
}

fn file_entry(path: &str, executable: bool, content: &[u8]) -> Vec<u8> {
  let mut bytes = vec![FILE_TAG];
  bytes.extend_from_slice(&(path.len() as u32).to_be_bytes());
  bytes.extend_from_slice(path.as_bytes());
  bytes.push(u8::from(executable));
  bytes.extend_from_slice(&(content.len() as u64).to_be_bytes());
  bytes.extend_from_slice(content);
  bytes
}

fn identity_descriptor(bytes: &[u8], entries: u64) -> BlobDescriptor {
  BlobDescriptor {
    digest: Digest::blake3(bytes),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: bytes.len() as u64,
    expanded_size_bytes: bytes.len() as u64,
    entry_count: entries,
  }
}

proptest! {
  #![proptest_config(ProptestConfig::with_cases(64))]

  #[test]
  fn arbitrary_identity_bundles_never_escape_or_panic(bytes in proptest::collection::vec(any::<u8>(), 1..4096)) {
    let descriptor = identity_descriptor(&bytes, 1);
    let base = TempDir::new().unwrap();
    let staging = base.path().join("staging");
    fs::create_dir(&staging).unwrap();
    let _ = extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      &staging,
      &[root("out")],
      BundleLimits {
        max_encoded_bytes: 4096,
        max_expanded_bytes: 4096,
        max_entries: 16,
        max_path_bytes: 256,
        max_file_bytes: 4096,
        max_compression_ratio: 4096,
        ..BundleLimits::default()
      },
      &CancellationToken::new(),
    );
    prop_assert_eq!(
      fs::read_dir(base.path()).unwrap().filter_map(Result::ok).filter(|entry| entry.path() != staging).count(),
      0
    );
  }

  #[test]
  fn valid_generated_file_entries_round_trip(
    segment in "[a-z][a-z0-9]{0,31}",
    content in proptest::collection::vec(any::<u8>(), 0..4096),
    executable in any::<bool>(),
  ) {
    let path = format!("out/{segment}");
    let mut entries = directory_entry("out");
    entries.extend(file_entry(&path, executable, &content));
    let bytes = raw_bundle(&entries);
    let descriptor = identity_descriptor(&bytes, 2);
    let staging = TempDir::new().unwrap();
    extract_bundle(
      Cursor::new(bytes),
      &descriptor,
      staging.path(),
      &[root("out")],
      BundleLimits::default(),
      &CancellationToken::new(),
    ).unwrap();
    prop_assert_eq!(fs::read(staging.path().join(path)).unwrap(), content);
  }
}
