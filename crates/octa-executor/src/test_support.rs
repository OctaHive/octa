//! Shared paths for executor tests that launch real workspace plugins.

use std::{env, path::PathBuf};

/// Returns a prepared directory containing the shell and template fixtures.
///
/// CI copies plugin binaries into the workspace `plugins` directory before
/// running tests. Custom test harnesses may instead provide an explicit
/// directory or build every binary beside the active Cargo test profile. The
/// lookup checks those sources by contents and never assumes `target/debug`,
/// which is incorrect for coverage and custom target directories.
pub(crate) fn plugin_directory() -> PathBuf {
  if let Some(directory) = env::var_os("OCTA_TEST_PLUGINS_DIR") {
    return PathBuf::from(directory);
  }

  let workspace_plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
  let executable = env::current_exe().expect("the current test executable must have a path");
  let deps = executable
    .parent()
    .expect("the current test executable must have a parent directory");
  let active_profile = deps
    .parent()
    .expect("the Cargo test deps directory must have a profile parent");

  [workspace_plugins, active_profile.to_path_buf()]
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
