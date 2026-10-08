use super::*;

#[test]
fn accepts_only_the_documented_version_shape() {
  assert_eq!(
    parse_version_output(b"codex-cli 0.161.0\n").unwrap(),
    Version::parse("0.161.0").unwrap()
  );

  for malformed in [
    b"".as_slice(),
    b"codex 0.161.0",
    b"codex-cli",
    b"codex-cli 0.161",
    b"codex-cli 0.161.0 extra",
    b"codex-cli 0.161.0.1",
    b"codex-cli 00.161.0",
    b"codex-cli 0.161.0-beta",
    b"codex-cli 18446744073709551616.161.0",
    &[0xff],
  ] {
    let error = parse_version_output(malformed).unwrap_err().to_string();
    assert!(error.len() < 160, "unbounded compatibility error: {error}");
  }
}

#[tokio::test]
async fn missing_or_relative_operator_selection_fails_without_path_search() {
  let cancellation = CancellationToken::new();
  for selection in [None, Some(OsString::new()), Some(OsString::from("codex"))] {
    let error = CodexExecutable::resolve_selected(selection, &cancellation)
      .await
      .err()
      .expect("selection must fail")
      .to_string();
    assert!(error.len() < 200, "unbounded compatibility error: {error}");
  }
}

#[tokio::test]
async fn tool_authorizer_requires_an_absolute_operator_selected_executable() {
  for selection in [None, Some(OsString::new()), Some(OsString::from("authorizer"))] {
    let error = ToolAuthorizer::resolve_selected(selection)
      .await
      .err()
      .expect("selection must fail")
      .to_string();
    assert!(error.len() < 200, "unbounded authorizer error: {error}");
  }
}

#[tokio::test]
async fn changed_tool_authorizer_fails_identity_revalidation() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory
    .path()
    .join(format!("authorizer{}", std::env::consts::EXE_SUFFIX));
  std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
  }
  let authorizer = ToolAuthorizer::resolve_selected(Some(path.clone().into_os_string()))
    .await
    .unwrap();
  use std::io::Write;
  std::fs::OpenOptions::new()
    .append(true)
    .open(path)
    .unwrap()
    .write_all(b"changed")
    .unwrap();

  let error = authorizer.ensure_unchanged().await.unwrap_err().to_string();
  assert!(error.contains("changed after validation"));
}

#[test]
fn directories_are_reported_as_non_regular_executables() {
  let directory = tempfile::tempdir().unwrap();

  let error = ExecutableFingerprint::read(directory.path(), CODEX_EXECUTABLE_DESCRIPTION)
    .err()
    .expect("directory must not be accepted as an executable")
    .to_string();

  assert!(error.contains("regular file"), "unexpected error: {error}");
  assert!(error.len() < 200, "unbounded compatibility error: {error}");
}

#[tokio::test]
async fn changed_executable_fails_identity_revalidation() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join(format!("codex{}", std::env::consts::EXE_SUFFIX));
  std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
  }

  let executable = CodexExecutable {
    fingerprint: ExecutableFingerprint::read(&path, CODEX_EXECUTABLE_DESCRIPTION).unwrap(),
    path: path.clone(),
    version: Version::parse(SUPPORTED_CODEX_VERSIONS[0]).unwrap(),
  };
  use std::io::Write;
  std::fs::OpenOptions::new()
    .append(true)
    .open(&path)
    .unwrap()
    .write_all(b"changed")
    .unwrap();

  let error = executable
    .fingerprint
    .ensure_unchanged(&executable.path, CODEX_EXECUTABLE_DESCRIPTION)
    .await
    .unwrap_err()
    .to_string();
  assert!(error.contains("changed after validation"));
}

#[test]
fn compatibility_document_lists_every_supported_version() {
  let documentation = include_str!("../../../../docs/codex-plugin.md");
  for version in SUPPORTED_CODEX_VERSIONS {
    assert!(
      documentation.contains(&format!("`{version}`")),
      "compatibility documentation omits {version}"
    );
  }
}
