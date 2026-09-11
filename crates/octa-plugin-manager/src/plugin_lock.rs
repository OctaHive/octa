//! Deterministic plugin metadata and digest verification.
//!
//! Manifests describe distributed binaries. A lock file records the exact
//! entrypoint and digest that a runner may start from its local plugin directory.

use std::{
  collections::BTreeMap,
  fmt::Write as _,
  path::{Component, Path, PathBuf},
};

use octa_plugin::protocol::PLUGIN_PROTOCOL_VERSION;
pub use octa_plugin_lock::{LockedPlugin, PluginLock, PLUGIN_LOCK_VERSION};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{fs::File, io::AsyncReadExt};

pub const PLUGIN_MANIFEST_VERSION: u8 = 1;

#[derive(Debug, Error)]
pub enum PluginLockError {
  #[error("failed to read plugin metadata '{path}': {source}")]
  Read { path: PathBuf, source: std::io::Error },
  #[error("failed to write plugin metadata '{path}': {source}")]
  Write { path: PathBuf, source: std::io::Error },
  #[error("failed to parse plugin metadata '{path}': {source}")]
  Parse {
    path: PathBuf,
    source: Box<serde_yml::Error>,
  },
  #[error("failed to serialize plugin metadata '{path}': {source}")]
  Serialize {
    path: PathBuf,
    source: Box<serde_yml::Error>,
  },
  #[error("unsupported plugin manifest version {0}")]
  ManifestVersion(u8),
  #[error("unsupported plugin lock version {0}")]
  LockVersion(u8),
  #[error("plugin '{0}' is absent from the lock file")]
  MissingPlugin(String),
  #[error("no '*.plugin.yml' manifests found in '{0}'")]
  NoManifests(PathBuf),
  #[error("duplicate plugin manifest name '{0}'")]
  DuplicatePlugin(String),
  #[error("invalid plugin manifest '{plugin}': {message}")]
  InvalidManifest { plugin: String, message: String },
  #[error("plugin '{plugin}' requires protocol {actual}, but Octa supports {expected}")]
  Protocol { plugin: String, actual: u16, expected: u16 },
  #[error("plugin '{plugin}' does not support platform '{platform}'")]
  Platform { plugin: String, platform: String },
  #[error("plugin '{plugin}' has an unsafe entrypoint '{entrypoint}'")]
  Entrypoint { plugin: String, entrypoint: PathBuf },
  #[error("plugin '{plugin}' entrypoint escapes the plugin directory")]
  EscapedEntrypoint { plugin: String },
  #[error("plugin '{plugin}' has invalid SHA-256 '{digest}'")]
  InvalidDigest { plugin: String, digest: String },
  #[error("plugin '{plugin}' digest mismatch: expected {expected}, got {actual}")]
  DigestMismatch {
    plugin: String,
    expected: String,
    actual: String,
  },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
  pub manifest_version: u8,
  pub name: String,
  pub version: String,
  pub protocol: u16,
  pub platforms: Vec<String>,
  pub entrypoint: PathBuf,
  pub sha256: String,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub capabilities: Vec<String>,
}

impl PluginManifest {
  /// Loads and validates one distribution manifest.
  pub fn load(path: &Path) -> Result<Self, PluginLockError> {
    let contents = std::fs::read_to_string(path).map_err(|source| PluginLockError::Read {
      path: path.to_path_buf(),
      source,
    })?;
    let manifest: Self = serde_yml::from_str(&contents).map_err(|source| PluginLockError::Parse {
      path: path.to_path_buf(),
      source: Box::new(source),
    })?;
    if manifest.manifest_version != PLUGIN_MANIFEST_VERSION {
      return Err(PluginLockError::ManifestVersion(manifest.manifest_version));
    }
    manifest.validate()?;
    Ok(manifest)
  }

  fn validate(&self) -> Result<(), PluginLockError> {
    if self.name.trim().is_empty() {
      return Err(PluginLockError::InvalidManifest {
        plugin: "(unnamed)".to_owned(),
        message: "name must not be empty".to_owned(),
      });
    }
    if self.version.trim().is_empty() || self.platforms.is_empty() {
      return Err(PluginLockError::InvalidManifest {
        plugin: self.name.clone(),
        message: "version and platforms must not be empty".to_owned(),
      });
    }
    if self.platforms.iter().any(|platform| platform.trim().is_empty())
      || self.capabilities.iter().any(|capability| capability.trim().is_empty())
    {
      return Err(PluginLockError::InvalidManifest {
        plugin: self.name.clone(),
        message: "platforms and capabilities must not contain empty values".to_owned(),
      });
    }
    Ok(())
  }

  pub fn into_locked(self, source: String) -> LockedPlugin {
    LockedPlugin {
      version: self.version,
      protocol: self.protocol,
      platforms: self.platforms,
      entrypoint: self.entrypoint,
      sha256: self.sha256,
      capabilities: self.capabilities,
      source,
    }
  }
}

/// Loads a versioned lock file without touching plugin binaries.
pub fn load_plugin_lock(path: &Path) -> Result<PluginLock, PluginLockError> {
  let contents = std::fs::read_to_string(path).map_err(|source| PluginLockError::Read {
    path: path.to_path_buf(),
    source,
  })?;
  let lock: PluginLock = serde_yml::from_str(&contents).map_err(|source| PluginLockError::Parse {
    path: path.to_path_buf(),
    source: Box::new(source),
  })?;
  if lock.version != PLUGIN_LOCK_VERSION {
    return Err(PluginLockError::LockVersion(lock.version));
  }
  Ok(lock)
}

/// Verifies one locked plugin and returns its canonical executable path.
pub async fn verify_plugin(lock: &PluginLock, plugin: &str, plugins_dir: &Path) -> Result<PathBuf, PluginLockError> {
  let entry = lock
    .plugins
    .get(plugin)
    .ok_or_else(|| PluginLockError::MissingPlugin(plugin.to_owned()))?;
  verify_locked(entry, plugin, plugins_dir).await
}

/// Returns metadata for one plugin without verifying its executable.
pub fn locked_plugin<'a>(lock: &'a PluginLock, plugin: &str) -> Result<&'a LockedPlugin, PluginLockError> {
  lock
    .plugins
    .get(plugin)
    .ok_or_else(|| PluginLockError::MissingPlugin(plugin.to_owned()))
}

/// Builds a deterministic lock from distribution manifests beside plugin binaries.
pub async fn lock_from_manifest_directory(plugins_dir: &Path) -> Result<PluginLock, PluginLockError> {
  let entries = std::fs::read_dir(plugins_dir).map_err(|source| PluginLockError::Read {
    path: plugins_dir.to_path_buf(),
    source,
  })?;
  let mut manifests = Vec::new();
  for entry in entries {
    let entry = entry.map_err(|source| PluginLockError::Read {
      path: plugins_dir.to_path_buf(),
      source,
    })?;
    let path = entry.path();
    if path
      .file_name()
      .and_then(|name| name.to_str())
      .is_some_and(|name| name.ends_with(".plugin.yml"))
    {
      manifests.push(path);
    }
  }
  manifests.sort();
  if manifests.is_empty() {
    return Err(PluginLockError::NoManifests(plugins_dir.to_path_buf()));
  }

  let mut plugins = BTreeMap::new();
  for path in manifests {
    let manifest = PluginManifest::load(&path)?;
    let name = manifest.name.clone();
    let source = path
      .file_name()
      .map(|name| name.to_string_lossy().into_owned())
      .unwrap_or_else(|| path.display().to_string());
    if plugins.insert(name.clone(), manifest.into_locked(source)).is_some() {
      return Err(PluginLockError::DuplicatePlugin(name));
    }
  }
  let lock = PluginLock {
    version: PLUGIN_LOCK_VERSION,
    plugins,
  };
  verify_plugin_lock(&lock, plugins_dir).await?;
  Ok(lock)
}

/// Verifies every entry in a lock file.
pub async fn verify_plugin_lock(lock: &PluginLock, plugins_dir: &Path) -> Result<Vec<PathBuf>, PluginLockError> {
  let mut verified = Vec::with_capacity(lock.plugins.len());
  for plugin in lock.plugins.keys() {
    verified.push(verify_plugin(lock, plugin, plugins_dir).await?);
  }
  Ok(verified)
}

/// Writes a lock file in deterministic mapping order.
pub fn write_plugin_lock(lock: &PluginLock, path: &Path) -> Result<(), PluginLockError> {
  let contents = serde_yml::to_string(lock).map_err(|source| PluginLockError::Serialize {
    path: path.to_path_buf(),
    source: Box::new(source),
  })?;
  std::fs::write(path, contents).map_err(|source| PluginLockError::Write {
    path: path.to_path_buf(),
    source,
  })
}

async fn verify_locked(locked: &LockedPlugin, plugin: &str, plugins_dir: &Path) -> Result<PathBuf, PluginLockError> {
  if plugin.trim().is_empty() || locked.version.trim().is_empty() || locked.platforms.is_empty() {
    return Err(PluginLockError::InvalidManifest {
      plugin: plugin.to_owned(),
      message: "name, version, and platforms must not be empty".to_owned(),
    });
  }
  if locked.protocol != PLUGIN_PROTOCOL_VERSION {
    return Err(PluginLockError::Protocol {
      plugin: plugin.to_owned(),
      actual: locked.protocol,
      expected: PLUGIN_PROTOCOL_VERSION,
    });
  }
  let platform = current_platform();
  if !locked.platforms.iter().any(|candidate| candidate == &platform) {
    return Err(PluginLockError::Platform {
      plugin: plugin.to_owned(),
      platform,
    });
  }
  if locked.entrypoint.is_absolute()
    || locked
      .entrypoint
      .components()
      .any(|component| !matches!(component, Component::Normal(_)))
  {
    return Err(PluginLockError::Entrypoint {
      plugin: plugin.to_owned(),
      entrypoint: locked.entrypoint.clone(),
    });
  }
  if locked.sha256.len() != 64 || !locked.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
    return Err(PluginLockError::InvalidDigest {
      plugin: plugin.to_owned(),
      digest: locked.sha256.clone(),
    });
  }

  let root = dunce::canonicalize(plugins_dir).map_err(|source| PluginLockError::Read {
    path: plugins_dir.to_path_buf(),
    source,
  })?;
  let executable = dunce::canonicalize(root.join(&locked.entrypoint)).map_err(|source| PluginLockError::Read {
    path: root.join(&locked.entrypoint),
    source,
  })?;
  if !executable.starts_with(&root) {
    return Err(PluginLockError::EscapedEntrypoint {
      plugin: plugin.to_owned(),
    });
  }
  let actual = sha256_file(&executable).await.map_err(|source| PluginLockError::Read {
    path: executable.clone(),
    source,
  })?;
  if !actual.eq_ignore_ascii_case(&locked.sha256) {
    return Err(PluginLockError::DigestMismatch {
      plugin: plugin.to_owned(),
      expected: locked.sha256.clone(),
      actual,
    });
  }
  Ok(executable)
}

pub fn current_platform() -> String {
  format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

pub async fn sha256_file(path: &Path) -> std::io::Result<String> {
  let mut file = File::open(path).await?;
  let mut digest = Sha256::new();
  let mut buffer = [0_u8; 64 * 1024];
  loop {
    let read = file.read(&mut buffer).await?;
    if read == 0 {
      break;
    }
    digest.update(&buffer[..read]);
  }
  let mut encoded = String::with_capacity(64);
  for byte in digest.finalize() {
    write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
  }
  Ok(encoded)
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;

  async fn locked_entry(directory: &Path) -> LockedPlugin {
    let executable = directory.join("plugin");
    tokio::fs::write(&executable, b"trusted").await.unwrap();
    LockedPlugin {
      version: "1.0.0".to_owned(),
      protocol: PLUGIN_PROTOCOL_VERSION,
      platforms: vec![current_platform()],
      entrypoint: "plugin".into(),
      sha256: sha256_file(&executable).await.unwrap(),
      capabilities: vec!["shell".to_owned()],
      source: "test.plugin.yml".to_owned(),
    }
  }

  #[tokio::test]
  async fn verifies_a_locked_binary_and_rejects_changes() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("plugin");
    tokio::fs::write(&executable, b"trusted").await.unwrap();
    let digest = sha256_file(&executable).await.unwrap();
    let lock = PluginLock {
      version: PLUGIN_LOCK_VERSION,
      plugins: BTreeMap::from([(
        "test".to_owned(),
        LockedPlugin {
          version: "1.0.0".to_owned(),
          protocol: PLUGIN_PROTOCOL_VERSION,
          platforms: vec![current_platform()],
          entrypoint: "plugin".into(),
          sha256: digest,
          capabilities: Vec::new(),
          source: "local".to_owned(),
        },
      )]),
    };

    assert_eq!(
      verify_plugin(&lock, "test", directory.path()).await.unwrap(),
      dunce::canonicalize(&executable).unwrap()
    );
    tokio::fs::write(&executable, b"modified").await.unwrap();
    assert!(matches!(
      verify_plugin(&lock, "test", directory.path()).await,
      Err(PluginLockError::DigestMismatch { .. })
    ));
  }

  #[tokio::test]
  async fn rejects_unsafe_entrypoints_before_reading_them() {
    let directory = tempfile::tempdir().unwrap();
    let lock = PluginLock {
      version: PLUGIN_LOCK_VERSION,
      plugins: BTreeMap::from([(
        "test".to_owned(),
        LockedPlugin {
          version: "1.0.0".to_owned(),
          protocol: PLUGIN_PROTOCOL_VERSION,
          platforms: vec![current_platform()],
          entrypoint: "../plugin".into(),
          sha256: "0".repeat(64),
          capabilities: Vec::new(),
          source: "local".to_owned(),
        },
      )]),
    };
    assert!(matches!(
      verify_plugin(&lock, "test", directory.path()).await,
      Err(PluginLockError::Entrypoint { .. })
    ));
  }

  #[test]
  fn manifest_loading_validates_versions_and_required_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("test.plugin.yml");
    assert!(matches!(PluginManifest::load(&path), Err(PluginLockError::Read { .. })));

    fs::write(&path, "not: [valid").unwrap();
    assert!(matches!(
      PluginManifest::load(&path),
      Err(PluginLockError::Parse { .. })
    ));

    let cases = [
      (
        "manifest_version: 2\nname: test\nversion: '1'\nprotocol: 1\nplatforms: [test]\nentrypoint: p\nsha256: x\n",
        "version",
      ),
      (
        "manifest_version: 1\nname: ' '\nversion: '1'\nprotocol: 1\nplatforms: [test]\nentrypoint: p\nsha256: x\n",
        "name",
      ),
      (
        "manifest_version: 1\nname: test\nversion: ''\nprotocol: 1\nplatforms: [test]\nentrypoint: p\nsha256: x\n",
        "must not be empty",
      ),
      (
        "manifest_version: 1\nname: test\nversion: '1'\nprotocol: 1\nplatforms: ['']\nentrypoint: p\nsha256: x\n",
        "empty",
      ),
    ];
    for (contents, expected) in cases {
      fs::write(&path, contents).unwrap();
      let error = PluginManifest::load(&path).unwrap_err();
      assert!(error.to_string().contains(expected), "unexpected error: {error}");
    }
  }

  #[tokio::test]
  async fn lock_round_trips_and_manifest_directory_is_deterministic() {
    let directory = tempfile::tempdir().unwrap();
    let entry = locked_entry(directory.path()).await;
    let manifest = PluginManifest {
      manifest_version: PLUGIN_MANIFEST_VERSION,
      name: "test".to_owned(),
      version: entry.version.clone(),
      protocol: entry.protocol,
      platforms: entry.platforms.clone(),
      entrypoint: entry.entrypoint.clone(),
      sha256: entry.sha256.clone(),
      capabilities: entry.capabilities.clone(),
    };
    fs::write(
      directory.path().join("test.plugin.yml"),
      serde_yml::to_string(&manifest).unwrap(),
    )
    .unwrap();

    let lock = lock_from_manifest_directory(directory.path()).await.unwrap();
    assert_eq!(locked_plugin(&lock, "test").unwrap().source, "test.plugin.yml");
    assert_eq!(verify_plugin_lock(&lock, directory.path()).await.unwrap().len(), 1);
    let path = directory.path().join("Octa.lock");
    write_plugin_lock(&lock, &path).unwrap();
    assert_eq!(load_plugin_lock(&path).unwrap(), lock);
    assert!(matches!(
      locked_plugin(&lock, "missing"),
      Err(PluginLockError::MissingPlugin(_))
    ));
    assert!(matches!(
      write_plugin_lock(&lock, directory.path()),
      Err(PluginLockError::Write { .. })
    ));
  }

  #[tokio::test]
  async fn rejects_missing_duplicate_and_incompatible_locked_plugins() {
    let empty = tempfile::tempdir().unwrap();
    assert!(matches!(
      lock_from_manifest_directory(empty.path()).await,
      Err(PluginLockError::NoManifests(_))
    ));
    assert!(matches!(
      lock_from_manifest_directory(&empty.path().join("missing")).await,
      Err(PluginLockError::Read { .. })
    ));

    let directory = tempfile::tempdir().unwrap();
    let entry = locked_entry(directory.path()).await;
    let lock = |entry: LockedPlugin| PluginLock {
      version: PLUGIN_LOCK_VERSION,
      plugins: BTreeMap::from([("test".to_owned(), entry)]),
    };

    let mut invalid = entry.clone();
    invalid.protocol += 1;
    assert!(matches!(
      verify_plugin(&lock(invalid), "test", directory.path()).await,
      Err(PluginLockError::Protocol { .. })
    ));
    let mut invalid = entry.clone();
    invalid.platforms = vec!["unsupported-platform".to_owned()];
    assert!(matches!(
      verify_plugin(&lock(invalid), "test", directory.path()).await,
      Err(PluginLockError::Platform { .. })
    ));
    let mut invalid = entry.clone();
    invalid.sha256 = "not-a-digest".to_owned();
    assert!(matches!(
      verify_plugin(&lock(invalid), "test", directory.path()).await,
      Err(PluginLockError::InvalidDigest { .. })
    ));
    let mut invalid = entry;
    invalid.version.clear();
    assert!(matches!(
      verify_plugin(&lock(invalid), "test", directory.path()).await,
      Err(PluginLockError::InvalidManifest { .. })
    ));

    let duplicate = tempfile::tempdir().unwrap();
    let entry = locked_entry(duplicate.path()).await;
    let manifest = PluginManifest {
      manifest_version: PLUGIN_MANIFEST_VERSION,
      name: "test".to_owned(),
      version: entry.version,
      protocol: entry.protocol,
      platforms: entry.platforms,
      entrypoint: entry.entrypoint,
      sha256: entry.sha256,
      capabilities: entry.capabilities,
    };
    let contents = serde_yml::to_string(&manifest).unwrap();
    fs::write(duplicate.path().join("a.plugin.yml"), &contents).unwrap();
    fs::write(duplicate.path().join("b.plugin.yml"), contents).unwrap();
    assert!(matches!(
      lock_from_manifest_directory(duplicate.path()).await,
      Err(PluginLockError::DuplicatePlugin(_))
    ));
  }

  #[test]
  fn lock_loading_rejects_invalid_data_and_versions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Octa.lock");
    assert!(matches!(load_plugin_lock(&path), Err(PluginLockError::Read { .. })));
    fs::write(&path, "not: [valid").unwrap();
    assert!(matches!(load_plugin_lock(&path), Err(PluginLockError::Parse { .. })));
    fs::write(&path, "version: 2\nplugins: {}\n").unwrap();
    assert!(matches!(load_plugin_lock(&path), Err(PluginLockError::LockVersion(2))));
  }

  #[tokio::test]
  async fn verification_reports_missing_directories_and_entrypoints() {
    let directory = tempfile::tempdir().unwrap();
    let entry = locked_entry(directory.path()).await;
    let lock = PluginLock {
      version: PLUGIN_LOCK_VERSION,
      plugins: BTreeMap::from([("test".to_owned(), entry)]),
    };
    assert!(matches!(
      verify_plugin(&lock, "test", &directory.path().join("missing")).await,
      Err(PluginLockError::Read { .. })
    ));
    fs::remove_file(directory.path().join("plugin")).unwrap();
    assert!(matches!(
      verify_plugin(&lock, "test", directory.path()).await,
      Err(PluginLockError::Read { .. })
    ));
    assert!(sha256_file(&directory.path().join("missing")).await.is_err());
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn rejects_entrypoint_symlinks_that_escape_the_plugin_directory() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    symlink(outside.path(), directory.path().join("plugin")).unwrap();
    let lock = PluginLock {
      version: PLUGIN_LOCK_VERSION,
      plugins: BTreeMap::from([(
        "test".to_owned(),
        LockedPlugin {
          version: "1".to_owned(),
          protocol: PLUGIN_PROTOCOL_VERSION,
          platforms: vec![current_platform()],
          entrypoint: "plugin".into(),
          sha256: "0".repeat(64),
          capabilities: Vec::new(),
          source: "test".to_owned(),
        },
      )]),
    };
    assert!(matches!(
      verify_plugin(&lock, "test", directory.path()).await,
      Err(PluginLockError::EscapedEntrypoint { .. })
    ));
  }
}
