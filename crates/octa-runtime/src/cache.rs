//! Runtime-owned loading and composition of task-result cache services.
//!
//! Octafile describes semantic inputs and outputs, while this module loads the
//! machine-specific storage and resource policy. Keeping the profile here lets
//! the interactive CLI and headless runner build the exact same `ResultCache`
//! without teaching the executor about configuration files.

use std::{
  fs,
  io::Read as _,
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use octa_cache::{
  BundleEncoding, BundleLimits, GarbageCollection, LocalCacheConfig, LocalCacheStatus, LocalCacheStore, RestoreManager,
  SnapshotOptions, DEFAULT_BUNDLE_COMPRESSION_LEVEL,
};
pub use octa_cache_protocol::CacheMode;
use octa_cache_protocol::{
  Digest, DigestAlgorithm, PlatformArchitecture, PlatformOs, RuntimeIdentity, MAX_CACHE_STRING_BYTES,
};
use octa_executor::ResultCache;
use serde::Deserialize;
use thiserror::Error;

const MAX_CACHE_PROFILE_BYTES: u64 = 256 * 1024;

/// Failure while decoding or composing an operational cache profile.
#[derive(Debug, Error)]
pub enum RuntimeCacheError {
  /// The profile could not be opened or read within its size bound.
  #[error("failed to read cache profile '{path}': {source}")]
  Read {
    /// Resolved profile path.
    path: PathBuf,
    /// Underlying filesystem failure.
    #[source]
    source: std::io::Error,
  },
  /// Strict TOML decoding failed.
  #[error("failed to parse cache profile '{path}': {source}")]
  Parse {
    /// Resolved profile path.
    path: PathBuf,
    /// Underlying TOML diagnostic.
    #[source]
    source: toml::de::Error,
  },
  /// A decoded operational value violates runtime invariants.
  #[error("invalid cache profile: {0}")]
  Invalid(String),
  /// Local store or restore-manager composition failed.
  #[error(transparent)]
  Cache(#[from] octa_cache::CacheError),
  /// Shared cache protocol identity validation failed.
  #[error(transparent)]
  Protocol(#[from] octa_cache_protocol::CacheProtocolError),
  /// The blocking cache initialization worker panicked or was cancelled.
  #[error("cache initialization worker failed: {0}")]
  Worker(#[source] tokio::task::JoinError),
}

/// Validated cache configuration independent of its source representation.
///
/// The runner constructs this value from its negotiated request, while the CLI
/// obtains it through [`RuntimeCacheConfig::load_profile`].
#[derive(Clone, Debug)]
pub struct RuntimeCacheConfig {
  mode: CacheMode,
  namespace: String,
  local: LocalCacheConfig,
  runtime: RuntimeIdentity,
  snapshot: SnapshotOptions,
  bundle_encoding: BundleEncoding,
  bundle_limits: BundleLimits,
}

impl RuntimeCacheConfig {
  /// Creates a local cache session with centralized production defaults.
  pub fn local(
    mode: CacheMode,
    namespace: impl Into<String>,
    directory: impl Into<PathBuf>,
    runtime: RuntimeIdentity,
  ) -> Result<Self, RuntimeCacheError> {
    let namespace = namespace.into();
    octa_cache::validate_namespace(&namespace)?;
    let directory = directory.into();
    if !directory.is_absolute() {
      return Err(RuntimeCacheError::Invalid(
        "local cache directory must be absolute after resolution".to_owned(),
      ));
    }
    runtime.validate()?;
    Ok(Self {
      mode,
      namespace,
      local: LocalCacheConfig::new(directory),
      runtime,
      snapshot: SnapshotOptions::default(),
      bundle_encoding: BundleEncoding::default(),
      bundle_limits: BundleLimits::default(),
    })
  }

  /// Loads a strict TOML profile resolved from `workspace`.
  ///
  /// The profile path may be relative; its local cache directory must be an
  /// explicit absolute operator-owned location.
  pub fn load_profile(path: &Path, workspace: &Path) -> Result<Self, RuntimeCacheError> {
    let path = resolve_path(workspace, path.to_path_buf());
    let metadata = fs::symlink_metadata(&path).map_err(|source| profile_read_error(&path, source))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_CACHE_PROFILE_BYTES
    {
      return Err(RuntimeCacheError::Invalid(format!(
        "cache profile '{}' must be a regular non-symlink file of at most {MAX_CACHE_PROFILE_BYTES} bytes",
        path.display()
      )));
    }
    let file = fs::File::open(&path).map_err(|source| profile_read_error(&path, source))?;
    let opened = file.metadata().map_err(|source| profile_read_error(&path, source))?;
    if !opened.is_file() || opened.len() > MAX_CACHE_PROFILE_BYTES {
      return Err(profile_changed_error(&path));
    }
    let current = fs::symlink_metadata(&path).map_err(|source| profile_read_error(&path, source))?;
    let same = same_file::Handle::from_file(file.try_clone().map_err(|source| profile_read_error(&path, source))?)
      .and_then(|opened| same_file::Handle::from_path(&path).map(|current| opened == current))
      .map_err(|source| profile_read_error(&path, source))?;
    if !same || !current.file_type().is_file() || current.file_type().is_symlink() {
      return Err(profile_changed_error(&path));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file
      .take(MAX_CACHE_PROFILE_BYTES + 1)
      .read_to_end(&mut bytes)
      .map_err(|source| profile_read_error(&path, source))?;
    if bytes.len() as u64 > MAX_CACHE_PROFILE_BYTES {
      return Err(RuntimeCacheError::Invalid(format!(
        "cache profile '{}' exceeds {MAX_CACHE_PROFILE_BYTES} bytes",
        path.display()
      )));
    }
    let text = std::str::from_utf8(&bytes)
      .map_err(|error| RuntimeCacheError::Invalid(format!("profile is not UTF-8: {error}")))?;
    let profile: CacheProfile = toml::from_str(text).map_err(|source| RuntimeCacheError::Parse { path, source })?;
    profile.resolve(workspace)
  }

  /// Builds the concrete local store, restore manager, and executor service.
  pub async fn open(&self) -> Result<ConfiguredCache, RuntimeCacheError> {
    let config = self.clone();
    tokio::task::spawn_blocking(move || config.open_sync())
      .await
      .map_err(RuntimeCacheError::Worker)?
  }

  fn open_sync(self) -> Result<ConfiguredCache, RuntimeCacheError> {
    let local = Arc::new(LocalCacheStore::open(self.local)?);
    let restore = RestoreManager::open(local.layout_root(), self.bundle_limits)?;
    restore.recover()?;
    let result_cache = ResultCache::new(local.clone(), restore, self.namespace, self.runtime)?
      .with_access(self.mode)
      .with_snapshot_options(self.snapshot)?
      .with_bundle_options(self.bundle_encoding, self.bundle_limits)?;
    Ok(ConfiguredCache {
      local,
      result_cache: Arc::new(result_cache),
    })
  }
}

/// Concrete cache services retained by a loaded runtime or management command.
#[derive(Clone)]
pub struct ConfiguredCache {
  local: Arc<LocalCacheStore>,
  result_cache: Arc<ResultCache>,
}

impl ConfiguredCache {
  /// Executor-facing task-result cache.
  pub fn result_cache(&self) -> Arc<ResultCache> {
    self.result_cache.clone()
  }

  /// Reports local capacity without exposing the physical store API upstream.
  pub async fn status(&self) -> Result<LocalCacheStatus, RuntimeCacheError> {
    Ok(self.local.status().await?)
  }

  /// Runs a forced local garbage-collection pass.
  pub async fn prune(&self) -> Result<GarbageCollection, RuntimeCacheError> {
    Ok(self.local.prune().await?)
  }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheProfile {
  mode: CacheMode,
  namespace: String,
  local: LocalProfile,
  environment: EnvironmentProfile,
  #[serde(default)]
  snapshot: SnapshotProfile,
  #[serde(default)]
  bundle: BundleProfile,
}

impl CacheProfile {
  fn resolve(self, workspace: &Path) -> Result<RuntimeCacheConfig, RuntimeCacheError> {
    if !workspace.is_absolute() {
      return Err(RuntimeCacheError::Invalid(
        "cache workspace must be absolute".to_owned(),
      ));
    }
    if !self.local.directory.is_absolute() {
      return Err(RuntimeCacheError::Invalid(
        "local cache directory must be an absolute operator-owned path".to_owned(),
      ));
    }
    let directory = self.local.directory.clone();
    let mut local = LocalCacheConfig::new(directory);
    apply_local_profile(&mut local, self.local);

    let mut snapshot = SnapshotOptions::default();
    apply_snapshot_profile(&mut snapshot, self.snapshot);
    let mut limits = BundleLimits::default();
    apply_bundle_profile(&mut limits, &self.bundle);
    let encoding = match self.bundle.compression {
      Compression::Identity => BundleEncoding::Identity,
      Compression::Zstd => BundleEncoding::ZstdV1 {
        level: self.bundle.compression_level,
      },
    };
    let runtime = native_runtime_identity(&self.environment.identity)?;
    let mut config = RuntimeCacheConfig::local(self.mode, self.namespace, local.root.clone(), runtime)?;
    config.local = local;
    config.snapshot = snapshot;
    config.bundle_encoding = encoding;
    config.bundle_limits = limits;
    Ok(config)
  }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalProfile {
  directory: PathBuf,
  max_bytes: Option<u64>,
  max_expanded_blob_bytes: Option<u64>,
  max_blob_compression_ratio: Option<u64>,
  max_entries: Option<usize>,
  high_watermark_bytes: Option<u64>,
  low_watermark_bytes: Option<u64>,
  temporary_grace_seconds: Option<u64>,
  access_update_interval_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentProfile {
  identity: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotProfile {
  max_parallel_hashes: Option<usize>,
  max_entries: Option<usize>,
  read_buffer_bytes: Option<usize>,
  mutation_retries: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Compression {
  Identity,
  #[default]
  Zstd,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BundleProfile {
  compression: Compression,
  compression_level: i32,
  max_encoded_bytes: Option<u64>,
  max_expanded_bytes: Option<u64>,
  max_entries: Option<u64>,
  max_path_bytes: Option<usize>,
  max_file_bytes: Option<u64>,
  max_compression_ratio: Option<u64>,
  read_buffer_bytes: Option<usize>,
}

impl Default for BundleProfile {
  fn default() -> Self {
    Self {
      compression: Compression::Zstd,
      compression_level: DEFAULT_BUNDLE_COMPRESSION_LEVEL,
      max_encoded_bytes: None,
      max_expanded_bytes: None,
      max_entries: None,
      max_path_bytes: None,
      max_file_bytes: None,
      max_compression_ratio: None,
      read_buffer_bytes: None,
    }
  }
}

fn apply_local_profile(config: &mut LocalCacheConfig, profile: LocalProfile) {
  macro_rules! assign {
    ($field:ident) => {
      if let Some(value) = profile.$field {
        config.$field = value;
      }
    };
  }
  assign!(max_bytes);
  assign!(max_expanded_blob_bytes);
  assign!(max_blob_compression_ratio);
  assign!(max_entries);
  assign!(high_watermark_bytes);
  assign!(low_watermark_bytes);
  if let Some(seconds) = profile.temporary_grace_seconds {
    config.temporary_grace = Duration::from_secs(seconds);
  }
  if let Some(seconds) = profile.access_update_interval_seconds {
    config.access_update_interval = Duration::from_secs(seconds);
  }
}

fn apply_snapshot_profile(options: &mut SnapshotOptions, profile: SnapshotProfile) {
  if let Some(value) = profile.max_parallel_hashes {
    options.max_parallel_hashes = value;
  }
  if let Some(value) = profile.max_entries {
    options.max_entries = value;
  }
  if let Some(value) = profile.read_buffer_bytes {
    options.read_buffer_bytes = value;
  }
  if let Some(value) = profile.mutation_retries {
    options.mutation_retries = value;
  }
}

fn apply_bundle_profile(limits: &mut BundleLimits, profile: &BundleProfile) {
  if let Some(value) = profile.max_encoded_bytes {
    limits.max_encoded_bytes = value;
  }
  if let Some(value) = profile.max_expanded_bytes {
    limits.max_expanded_bytes = value;
  }
  if let Some(value) = profile.max_entries {
    limits.max_entries = value;
  }
  if let Some(value) = profile.max_path_bytes {
    limits.max_path_bytes = value;
  }
  if let Some(value) = profile.max_file_bytes {
    limits.max_file_bytes = value;
  }
  if let Some(value) = profile.max_compression_ratio {
    limits.max_compression_ratio = value;
  }
  if let Some(value) = profile.read_buffer_bytes {
    limits.read_buffer_bytes = value;
  }
}

fn resolve_path(workspace: &Path, path: PathBuf) -> PathBuf {
  if path.is_absolute() {
    path
  } else {
    workspace.join(path)
  }
}

fn profile_read_error(path: &Path, source: std::io::Error) -> RuntimeCacheError {
  RuntimeCacheError::Read {
    path: path.to_path_buf(),
    source,
  }
}

fn profile_changed_error(path: &Path) -> RuntimeCacheError {
  RuntimeCacheError::Invalid(format!(
    "cache profile '{}' changed while it was being opened",
    path.display()
  ))
}

fn native_runtime_identity(identity: &str) -> Result<RuntimeIdentity, RuntimeCacheError> {
  if identity.is_empty() || identity.len() > MAX_CACHE_STRING_BYTES || identity.chars().any(char::is_control) {
    return Err(RuntimeCacheError::Invalid(format!(
      "environment.identity must contain 1 to {MAX_CACHE_STRING_BYTES} UTF-8 bytes and no control characters"
    )));
  }
  let digest = Digest::new(
    DigestAlgorithm::Blake3,
    *blake3::hash(identity.as_bytes()).as_bytes(),
    identity.len() as u64,
  );
  Ok(RuntimeIdentity::Native {
    os: host_os()?,
    architecture: host_architecture()?,
    environment: digest,
  })
}

fn host_os() -> Result<PlatformOs, RuntimeCacheError> {
  match std::env::consts::OS {
    "linux" => Ok(PlatformOs::Linux),
    "windows" => Ok(PlatformOs::Windows),
    "macos" => Ok(PlatformOs::Macos),
    value => Err(RuntimeCacheError::Invalid(format!(
      "task-result caching does not support host operating system '{value}'"
    ))),
  }
}

fn host_architecture() -> Result<PlatformArchitecture, RuntimeCacheError> {
  match std::env::consts::ARCH {
    "x86_64" => Ok(PlatformArchitecture::Amd64),
    "aarch64" => Ok(PlatformArchitecture::Arm64),
    value => Err(RuntimeCacheError::Invalid(format!(
      "task-result caching does not support host architecture '{value}'"
    ))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  #[test]
  fn loads_defaults_with_an_absolute_operator_cache_directory() {
    let workspace = TempDir::new().unwrap();
    let cache = workspace.path().join("external-cache");
    let path = workspace.path().join("cache.toml");
    fs::write(
      &path,
      format!(
        r#"
mode = "read_write"
namespace = "project/example"

[local]
directory = "{}"

[environment]
identity = "rust-1.98-toolchain-v1"
"#,
        cache.to_string_lossy().replace('\\', "/")
      ),
    )
    .unwrap();

    let config = RuntimeCacheConfig::load_profile(&path, workspace.path()).unwrap();
    assert_eq!(config.mode, CacheMode::ReadWrite);
    assert_eq!(config.local.root, cache);
    assert_eq!(config.bundle_encoding, BundleEncoding::ZstdV1 { level: 3 });
    assert!(matches!(
      config.runtime,
      RuntimeIdentity::Native { environment, .. }
        if environment == Digest::blake3(b"rust-1.98-toolchain-v1")
    ));
  }

  #[test]
  fn rejects_unknown_and_invalid_profile_values() {
    let workspace = TempDir::new().unwrap();
    let unknown = workspace.path().join("unknown.toml");
    fs::write(
      &unknown,
      "mode = 'read_write'\nnamespace = 'n'\nunknown = true\n[local]\ndirectory = 'c'\n[environment]\nidentity = 'e'\n",
    )
    .unwrap();
    assert!(matches!(
      RuntimeCacheConfig::load_profile(&unknown, workspace.path()),
      Err(RuntimeCacheError::Parse { .. })
    ));

    let invalid = workspace.path().join("invalid.toml");
    fs::write(
      &invalid,
      "mode = 'read_write'\nnamespace = ''\n[local]\ndirectory = 'c'\n[environment]\nidentity = 'e'\n",
    )
    .unwrap();
    assert!(RuntimeCacheConfig::load_profile(&invalid, workspace.path()).is_err());
  }

  #[test]
  fn applies_every_operational_profile_override() {
    let workspace = TempDir::new().unwrap();
    let cache = workspace.path().join("cache");
    let path = workspace.path().join("complete.toml");
    fs::write(
      &path,
      format!(
        r#"
mode = "write_only"
namespace = "project/complete"

[local]
directory = "{}"
max_bytes = 10000
max_expanded_blob_bytes = 6000
max_blob_compression_ratio = 10
max_entries = 100
high_watermark_bytes = 9000
low_watermark_bytes = 8000
temporary_grace_seconds = 12
access_update_interval_seconds = 13

[environment]
identity = "complete-toolchain"

[snapshot]
max_parallel_hashes = 2
max_entries = 90
read_buffer_bytes = 4096
mutation_retries = 4

[bundle]
compression = "identity"
compression_level = 1
max_encoded_bytes = 5000
max_expanded_bytes = 6000
max_entries = 80
max_path_bytes = 1024
max_file_bytes = 4000
max_compression_ratio = 8
read_buffer_bytes = 4096
"#,
        cache.to_string_lossy().replace('\\', "/")
      ),
    )
    .unwrap();

    let config = RuntimeCacheConfig::load_profile(&path, workspace.path()).unwrap();
    assert_eq!(config.mode, CacheMode::WriteOnly);
    assert_eq!(config.namespace, "project/complete");
    assert_eq!(config.local.max_bytes, 10_000);
    assert_eq!(config.local.max_expanded_blob_bytes, 6_000);
    assert_eq!(config.local.max_blob_compression_ratio, 10);
    assert_eq!(config.local.max_entries, 100);
    assert_eq!(config.local.high_watermark_bytes, 9_000);
    assert_eq!(config.local.low_watermark_bytes, 8_000);
    assert_eq!(config.local.temporary_grace, Duration::from_secs(12));
    assert_eq!(config.local.access_update_interval, Duration::from_secs(13));
    assert_eq!(config.snapshot.max_parallel_hashes, 2);
    assert_eq!(config.snapshot.max_entries, 90);
    assert_eq!(config.snapshot.read_buffer_bytes, 4096);
    assert_eq!(config.snapshot.mutation_retries, 4);
    assert_eq!(config.bundle_encoding, BundleEncoding::Identity);
    assert_eq!(config.bundle_limits.max_encoded_bytes, 5_000);
    assert_eq!(config.bundle_limits.max_expanded_bytes, 6_000);
    assert_eq!(config.bundle_limits.max_entries, 80);
    assert_eq!(config.bundle_limits.max_path_bytes, 1024);
    assert_eq!(config.bundle_limits.max_file_bytes, 4_000);
    assert_eq!(config.bundle_limits.max_compression_ratio, 8);
    assert_eq!(config.bundle_limits.read_buffer_bytes, 4096);
  }

  #[test]
  fn rejects_unsafe_profile_inputs_and_runtime_identities() {
    let workspace = TempDir::new().unwrap();
    let missing = workspace.path().join("missing.toml");
    assert!(matches!(
      RuntimeCacheConfig::load_profile(&missing, workspace.path()),
      Err(RuntimeCacheError::Read { .. })
    ));

    let directory = workspace.path().join("profile-directory");
    fs::create_dir(&directory).unwrap();
    assert!(matches!(
      RuntimeCacheConfig::load_profile(&directory, workspace.path()),
      Err(RuntimeCacheError::Invalid(_))
    ));

    let oversized = workspace.path().join("oversized.toml");
    fs::write(&oversized, vec![b'x'; MAX_CACHE_PROFILE_BYTES as usize + 1]).unwrap();
    assert!(matches!(
      RuntimeCacheConfig::load_profile(&oversized, workspace.path()),
      Err(RuntimeCacheError::Invalid(_))
    ));

    let binary = workspace.path().join("binary.toml");
    fs::write(&binary, [0xff]).unwrap();
    assert!(matches!(
      RuntimeCacheConfig::load_profile(&binary, workspace.path()),
      Err(RuntimeCacheError::Invalid(_))
    ));

    let profile = workspace.path().join("relative-workspace.toml");
    fs::write(
      &profile,
      "mode='read_only'\nnamespace='n'\n[local]\ndirectory='cache'\n[environment]\nidentity='e'\n",
    )
    .unwrap();
    assert!(matches!(
      RuntimeCacheConfig::load_profile(&profile, Path::new("relative")),
      Err(RuntimeCacheError::Invalid(_))
    ));

    let absolute = workspace.path().join("cache");
    let invalid_native = RuntimeIdentity::Native {
      os: PlatformOs::Linux,
      architecture: PlatformArchitecture::Amd64,
      environment: Digest::new(DigestAlgorithm::Sha256, [1; 32], 1),
    };
    assert!(RuntimeCacheConfig::local(CacheMode::ReadWrite, "n", &absolute, invalid_native).is_err());
    let invalid_oci = RuntimeIdentity::Oci {
      os: PlatformOs::Linux,
      architecture: PlatformArchitecture::Amd64,
      image: Digest::blake3(b"image"),
    };
    assert!(RuntimeCacheConfig::local(CacheMode::ReadWrite, "n", &absolute, invalid_oci).is_err());
    let macos_oci = RuntimeIdentity::Oci {
      os: PlatformOs::Macos,
      architecture: PlatformArchitecture::Arm64,
      image: Digest::new(DigestAlgorithm::Sha256, [2; 32], 1),
    };
    assert!(RuntimeCacheConfig::local(CacheMode::ReadWrite, "n", &absolute, macos_oci).is_err());
    assert!(RuntimeCacheConfig::local(
      CacheMode::ReadWrite,
      "n",
      PathBuf::from("relative"),
      native_runtime_identity("valid").unwrap(),
    )
    .is_err());
    assert!(native_runtime_identity("").is_err());
  }

  #[cfg(unix)]
  #[test]
  fn rejects_a_symlinked_profile() {
    use std::os::unix::fs::symlink;

    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("target.toml");
    fs::write(&target, "not followed").unwrap();
    let link = workspace.path().join("profile.toml");
    symlink(target, &link).unwrap();
    assert!(matches!(
      RuntimeCacheConfig::load_profile(&link, workspace.path()),
      Err(RuntimeCacheError::Invalid(_))
    ));
  }

  #[tokio::test]
  async fn composes_status_and_prune_from_the_same_configuration() {
    let workspace = TempDir::new().unwrap();
    let config = RuntimeCacheConfig {
      mode: CacheMode::ReadWrite,
      namespace: "project/test".to_owned(),
      local: LocalCacheConfig::new(workspace.path().join("cache")),
      runtime: native_runtime_identity("test-environment").unwrap(),
      snapshot: SnapshotOptions::default(),
      bundle_encoding: BundleEncoding::Identity,
      bundle_limits: BundleLimits::default(),
    };
    let cache = config.open().await.unwrap();
    assert_eq!(cache.status().await.unwrap().max_bytes, config.local.max_bytes);
    assert_eq!(
      cache.prune().await.unwrap().bytes_after,
      cache.status().await.unwrap().used_bytes
    );
  }
}
