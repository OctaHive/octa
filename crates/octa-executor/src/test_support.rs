//! Shared paths for executor tests that launch real workspace plugins.

use std::{env, path::PathBuf};

/// Returns a prepared directory containing the shell and template fixtures.
///
/// CI copies plugin binaries into the workspace `plugins` directory before
/// running tests. Custom test harnesses may instead provide an explicit
/// directory or build every binary beside the active Cargo test profile. The
/// normal Cargo target is a fallback for coverage, which builds test harnesses
/// in a private target but uses the separately prepared plugin executables.
pub(crate) fn plugin_directory() -> PathBuf {
  if let Some(directory) = env::var_os("OCTA_TEST_PLUGINS_DIR") {
    return PathBuf::from(directory);
  }

  let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
  let workspace_plugins = workspace.join("plugins");
  let default_profile = workspace.join("target/debug");
  let executable = env::current_exe().expect("the current test executable must have a path");
  let deps = executable
    .parent()
    .expect("the current test executable must have a parent directory");
  let active_profile = deps
    .parent()
    .expect("the Cargo test deps directory must have a profile parent");

  // Prefer the binary produced by this Cargo invocation. The workspace
  // directory is a CI packaging fixture and can legitimately contain an
  // older locally copied plugin from another build.
  [active_profile.to_path_buf(), default_profile, workspace_plugins]
    .into_iter()
    .find(|directory| required_plugins_exist(directory))
    .expect("shell and template test plugins must be built before executor tests")
}

fn required_plugins_exist(directory: &std::path::Path) -> bool {
  #[cfg(windows)]
  let names = ["octa_plugin_shell.exe", "octa_plugin_tpl.exe"];
  #[cfg(not(windows))]
  let names = ["octa_plugin_shell", "octa_plugin_tpl"];
  names.into_iter().all(|name| directory.join(name).is_file())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn locates_a_complete_fixture_directory_without_a_fixed_target_path() {
    let directory = plugin_directory();
    assert!(required_plugins_exist(&directory));
  }
}
