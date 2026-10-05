//! Shared installation helpers for black-box Codex plugin tests.

use std::{fs, path::Path, path::PathBuf};

/// Copies the fixture under the production executable name expected by tests.
pub(crate) fn install_process_fixture(directory: &Path) -> PathBuf {
  let source = Path::new(env!("CARGO_BIN_EXE_codex-test-fixture"));
  let target = directory.join(format!("codex{}", std::env::consts::EXE_SUFFIX));
  fs::copy(source, &target).expect("copy Codex process fixture");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("make Codex process fixture executable");
  }
  target
}

/// Installs a fixture and selects one deterministic runtime scenario.
pub(crate) fn configured_process_fixture(directory: &Path, mode: &str) -> PathBuf {
  let executable = install_process_fixture(directory);
  fs::write(executable.with_extension("run-mode"), mode).expect("write fixture run mode");
  executable
}
