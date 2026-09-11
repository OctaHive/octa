//! Versioned, serialization-only schema for `Octa.lock`.
//!
//! The schema is intentionally independent of plugin discovery and execution
//! so external consumers such as OctaCity can validate the exact same wire
//! format without depending on the plugin manager.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Current `Octa.lock` document version.
pub const PLUGIN_LOCK_VERSION: u8 = 1;

/// Exact plugin set authorized by an `Octa.lock` document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginLock {
  /// Lock-file schema version.
  pub version: u8,
  /// Plugins indexed by their task-type name.
  pub plugins: BTreeMap<String, LockedPlugin>,
}

/// Immutable identity and executable metadata for one locked plugin.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPlugin {
  /// Plugin package version.
  pub version: String,
  /// Plugin process-protocol version.
  pub protocol: u16,
  /// Host platforms supported by this executable.
  pub platforms: Vec<String>,
  /// Executable path relative to the plugin directory.
  pub entrypoint: PathBuf,
  /// Lowercase SHA-256 digest of the executable.
  pub sha256: String,
  /// Semantic capabilities advertised to Octa and its UI.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub capabilities: Vec<String>,
  /// Operator-visible source from which the plugin was installed.
  pub source: String,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lock_schema_round_trips_and_defaults_capabilities() {
    let source = "version: 1\nplugins:\n  shell:\n    version: 0.3.0\n    protocol: 1\n    platforms: [linux-x86_64]\n    entrypoint: octa_plugin_shell\n    sha256: abc\n    source: shell.plugin.yml\n";
    let lock: PluginLock = serde_yml::from_str(source).unwrap();
    assert!(lock.plugins["shell"].capabilities.is_empty());

    let encoded = serde_yml::to_string(&lock).unwrap();
    assert!(!encoded.contains("capabilities:"));
    assert_eq!(serde_yml::from_str::<PluginLock>(&encoded).unwrap(), lock);
  }

  #[test]
  fn lock_schema_rejects_unknown_fields() {
    let source = "version: 1\nplugins: {}\nunknown: true\n";
    assert!(serde_yml::from_str::<PluginLock>(source).is_err());
  }
}
