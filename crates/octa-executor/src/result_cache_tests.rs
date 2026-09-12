use std::{
  collections::HashSet,
  fs,
  path::{Path, PathBuf},
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  },
};

use octa_cache::{ActionLookup, BlobReader, CacheError, CacheStore, LocalCacheConfig, LocalCacheStore, WriteOutcome};
use octa_cache_protocol::{
  ActionResultV1, BlobDescriptor, BlobEncoding, CacheLayer, CachedArtifact, CachedReport, Digest, DigestAlgorithm,
  PlatformArchitecture, PlatformOs, ACTION_RESULT_VERSION_V1, MAX_ACTION_RESULT_METADATA_BYTES,
};
use octa_output::{CacheReason, Console, ConsoleScopeAllocator, RegisteredArtifact, RegisteredReport};
use serde_json::{json, Map};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
  error::ExecutorError, execution_result::CacheStatus, runtime_output::RuntimeOutput, structured_output::TaskOutputs,
  task::RuntimeContext,
};
use identity::{action_digest, relative_directory};
use replay::{
  action_result, cached_result, validate_materialized_resources, validate_registered_resources,
  validate_resource_contract,
};

/// Deterministic store used to exercise executor fallback policy without
/// depending on filesystem corruption in the local CAS implementation.
struct ControlledStore {
  action: Mutex<Option<ActionResultV1>>,
  blob: Option<Vec<u8>>,
  blob_missing: bool,
  blob_lookup_fails: bool,
  invalid_missing_response: bool,
  blob_read_fails: bool,
  blob_write: Result<WriteOutcome, &'static str>,
  action_write: Result<WriteOutcome, &'static str>,
  mutate_input_on_lookup: Option<PathBuf>,
  remove_workspace_on_lookup: Option<PathBuf>,
  cancel_on_lookup: Option<CancellationToken>,
  cancel_on_blob_read: Option<CancellationToken>,
  action_read_fails: bool,
  lookups: AtomicUsize,
  blob_writes: AtomicUsize,
  action_writes: AtomicUsize,
}

impl Default for ControlledStore {
  fn default() -> Self {
    Self {
      action: Mutex::new(None),
      blob: None,
      blob_missing: true,
      blob_lookup_fails: false,
      invalid_missing_response: false,
      blob_read_fails: false,
      blob_write: Ok(WriteOutcome::Written),
      action_write: Ok(WriteOutcome::Written),
      mutate_input_on_lookup: None,
      remove_workspace_on_lookup: None,
      cancel_on_lookup: None,
      cancel_on_blob_read: None,
      action_read_fails: false,
      lookups: AtomicUsize::new(0),
      blob_writes: AtomicUsize::new(0),
      action_writes: AtomicUsize::new(0),
    }
  }
}

#[async_trait::async_trait]
impl CacheStore for ControlledStore {
  async fn get_action(&self, _namespace: &str, _action: &Digest) -> Result<Option<ActionLookup>, CacheError> {
    self.lookups.fetch_add(1, Ordering::Relaxed);
    if self.action_read_fails {
      return Err(CacheError::Configuration("offline".to_owned()));
    }
    if let Some(path) = &self.mutate_input_on_lookup {
      fs::write(path, "changed during lookup")?;
    }
    if let Some(path) = &self.remove_workspace_on_lookup {
      fs::remove_dir_all(path)?;
    }
    if let Some(cancel) = &self.cancel_on_lookup {
      cancel.cancel();
    }
    Ok(self.action.lock().await.clone().map(|result| ActionLookup {
      result,
      layer: CacheLayer::Local,
    }))
  }

  async fn find_missing_blobs(&self, blobs: &[BlobDescriptor]) -> Result<Vec<BlobDescriptor>, CacheError> {
    if self.blob_lookup_fails {
      return Err(CacheError::Configuration("blob lookup unavailable".to_owned()));
    }
    if self.invalid_missing_response {
      let mut invalid = blobs.to_vec();
      invalid.extend_from_slice(blobs);
      return Ok(invalid);
    }
    Ok(if self.blob_missing { blobs.to_vec() } else { Vec::new() })
  }

  async fn read_blob(&self, _blob: &BlobDescriptor) -> Result<BlobReader, CacheError> {
    if self.blob_read_fails {
      return Err(CacheError::Configuration("blob unavailable".to_owned()));
    }
    if let Some(cancel) = &self.cancel_on_blob_read {
      cancel.cancel();
    }
    let bytes = self.blob.clone().unwrap_or_default();
    Ok(Box::pin(std::io::Cursor::new(bytes)))
  }

  async fn write_blob_if_absent(&self, _blob: &BlobDescriptor, _body: BlobReader) -> Result<WriteOutcome, CacheError> {
    self.blob_writes.fetch_add(1, Ordering::Relaxed);
    self
      .blob_write
      .map_err(|message| CacheError::Configuration(message.to_owned()))
  }

  async fn write_action_if_absent(
    &self,
    _namespace: &str,
    _result: &ActionResultV1,
  ) -> Result<WriteOutcome, CacheError> {
    self.action_writes.fetch_add(1, Ordering::Relaxed);
    self
      .action_write
      .map_err(|message| CacheError::Configuration(message.to_owned()))
  }
}

fn artifact(path: &str) -> RegisteredArtifact {
  RegisteredArtifact {
    name: "application".to_owned(),
    path: path.to_owned(),
    content_type: Some("application/octet-stream".to_owned()),
  }
}

fn report() -> RegisteredReport {
  RegisteredReport {
    name: "tests".to_owned(),
    path: "reports/junit.xml".to_owned(),
    format: "junit".to_owned(),
  }
}

fn plan(workspace: &Path, inputs: Vec<String>, outputs: Vec<RelativePath>) -> TaskCachePlan {
  TaskCachePlan {
    workspace: workspace.to_path_buf(),
    inputs,
    outputs,
    task_definition: Digest::blake3(b"task"),
    environment: Vec::new(),
    plugin_keys: Vec::new(),
    arguments: Vec::new(),
    timeout: None,
    salt: None,
  }
}

fn runtime_context(workspace: &Path) -> RuntimeContext {
  RuntimeContext {
    vars: crate::vars::Vars::new(),
    envs: crate::envs::Envs::new(),
    dir: workspace.to_path_buf(),
    identity_names: HashSet::new(),
    plugin_uses: Arc::new(std::sync::Mutex::new(HashSet::new())),
  }
}

fn runtime_cache(workspace: &Path, store: Arc<dyn CacheStore>) -> ResultCache {
  let restore = RestoreManager::open(workspace.join("cache-state"), BundleLimits::default()).unwrap();
  ResultCache::new(
    store,
    restore,
    "test",
    RuntimeIdentity::Native {
      os: PlatformOs::Macos,
      architecture: PlatformArchitecture::Arm64,
      environment: Digest::blake3(b"toolchain"),
    },
  )
  .unwrap()
}

fn runtime_output() -> RuntimeOutput {
  RuntimeOutput::new(
    Arc::new(Console::default()),
    1,
    Some(ConsoleScopeAllocator::default().scope("task")),
  )
}

async fn action_for(
  cache: &ResultCache,
  plan: &TaskCachePlan,
  manager: &octa_plugin_manager::plugin_manager::PluginManager,
  context: &RuntimeContext,
) -> Digest {
  let root = cache
    .snapshotter
    .snapshot(&plan.workspace, &plan.inputs, &CancellationToken::new())
    .await
    .unwrap()
    .root;
  action_digest(cache, plan, manager, context, root).await.unwrap()
}

#[test]
fn cache_configuration_identity_and_result_metadata_are_stable() {
  let directory = TempDir::new().unwrap();
  let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(directory.path().join("cache"))).unwrap());
  let restore = RestoreManager::open(store.layout_root(), BundleLimits::default()).unwrap();
  assert!(matches!(
    ResultCache::new(
      store.clone(),
      restore.clone(),
      "",
      RuntimeIdentity::Native {
        os: PlatformOs::Macos,
        architecture: PlatformArchitecture::Arm64,
        environment: Digest::blake3(b"toolchain"),
      },
    ),
    Err(CacheError::Protocol(_))
  ));
  let cache = ResultCache::new(
    store,
    restore,
    "test",
    RuntimeIdentity::Native {
      os: PlatformOs::Macos,
      architecture: PlatformArchitecture::Arm64,
      environment: Digest::blake3(b"toolchain"),
    },
  )
  .unwrap()
  .with_snapshot_options(SnapshotOptions::default())
  .unwrap()
  .with_bundle_options(BundleEncoding::Identity, BundleLimits::default())
  .unwrap();
  assert!(matches!(
    cache
      .clone()
      .with_bundle_options(BundleEncoding::ZstdV1 { level: 23 }, BundleLimits::default()),
    Err(CacheError::Configuration(_))
  ));
  assert!(matches!(
    cache.clone().with_bundle_options(
      BundleEncoding::Identity,
      BundleLimits {
        max_entries: 0,
        ..BundleLimits::default()
      }
    ),
    Err(CacheError::Configuration(_))
  ));
  let debug = format!("{cache:?}");
  assert!(debug.contains("test") && debug.contains("Identity"));

  let left = TaskCachePlan::definition_digest(&json!({"b": 2, "a": 1})).unwrap();
  let right = TaskCachePlan::definition_digest(&json!({"a": 1, "b": 2})).unwrap();
  assert_eq!(left, right);

  let mut task = TaskOutputs::default();
  task.insert("public".to_owned(), json!(42), false);
  task.insert("secret".to_owned(), json!("hidden"), true);
  let captured = CapturedResult {
    stdout: "compiled".to_owned(),
    outputs: CompletionOutputs::new(Map::new(), task).with_resources(vec![artifact("dist/app")], vec![report()]),
  };
  let bundle = BlobDescriptor {
    digest: Digest::blake3(b"x"),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: 1,
    expanded_size_bytes: 1,
    entry_count: 1,
  };
  let result = action_result(Digest::blake3(b"action"), Some(bundle), &captured).unwrap();
  assert_eq!(result.task_outputs, BTreeMap::from([("public".to_owned(), json!(42))]));
  fs::create_dir_all(directory.path().join("dist")).unwrap();
  fs::write(directory.path().join("dist/app"), "application").unwrap();
  fs::create_dir_all(directory.path().join("reports")).unwrap();
  fs::write(directory.path().join("reports/junit.xml"), "<testsuites/>").unwrap();
  let replay_plan = plan(
    directory.path(),
    Vec::new(),
    vec![
      RelativePath::new("dist").unwrap(),
      RelativePath::new("reports").unwrap(),
    ],
  );
  validate_resource_contract(&replay_plan, &result.artifacts, &result.reports).unwrap();
  let restored = cached_result(result);
  assert_eq!(restored.stdout, "compiled");
  assert_eq!(restored.outputs.artifacts(), [artifact("dist/app")]);
  assert_eq!(restored.outputs.reports(), [report()]);

  let invalid = CapturedResult {
    stdout: String::new(),
    outputs: CompletionOutputs::default().with_resources(vec![artifact("../escape")], Vec::new()),
  };
  assert!(action_result(Digest::blake3(b"action"), None, &invalid).is_err());
  let oversized = CapturedResult {
    stdout: "x".repeat(MAX_ACTION_RESULT_METADATA_BYTES + 1),
    outputs: CompletionOutputs::default(),
  };
  assert!(action_result(Digest::blake3(b"action"), None, &oversized).is_err());
}

#[test]
fn aggregation_and_relative_directories_preserve_task_semantics() {
  let directory = TempDir::new().unwrap();
  let child = directory.path().join("child");
  fs::create_dir(&child).unwrap();
  assert!(relative_directory(directory.path(), directory.path())
    .unwrap()
    .is_root());
  assert_eq!(relative_directory(directory.path(), &child).unwrap().as_str(), "child");
  let outside = TempDir::new().unwrap();
  assert!(relative_directory(directory.path(), outside.path()).is_err());

  let mut first_outputs = TaskOutputs::default();
  first_outputs.insert("first".to_owned(), json!(1), false);
  let mut second_outputs = TaskOutputs::default();
  second_outputs.insert("second".to_owned(), json!(2), false);
  let captures = BTreeMap::from([
    (
      0,
      CapturedResult {
        stdout: "first".to_owned(),
        outputs: CompletionOutputs::new(Map::new(), first_outputs).with_resources(vec![artifact("dist/app")], vec![]),
      },
    ),
    (
      1,
      CapturedResult {
        stdout: "second".to_owned(),
        outputs: CompletionOutputs::new(Map::new(), second_outputs)
          .with_resources(vec![artifact("dist/app")], vec![report()]),
      },
    ),
  ]);
  let resources = CompletionOutputs::default().with_resources(vec![artifact("dist/app")], vec![report()]);
  let aggregated = aggregate(&captures, Some(&resources));
  assert_eq!(aggregated.stdout, "first\nsecond");
  assert_eq!(aggregated.outputs.task().public_values().len(), 2);
  assert_eq!(aggregated.outputs.artifacts().len(), 1);
  assert_eq!(aggregated.outputs.reports().len(), 1);
}

#[test]
fn cache_resources_must_be_replayable_from_declared_outputs() {
  let directory = TempDir::new().unwrap();
  fs::create_dir(directory.path().join("dist")).unwrap();
  fs::write(directory.path().join("dist/app"), "application").unwrap();
  fs::write(directory.path().join("source.txt"), "source").unwrap();
  let plan = plan(directory.path(), Vec::new(), vec![RelativePath::new("dist").unwrap()]);

  let outside = CapturedResult {
    stdout: String::new(),
    outputs: CompletionOutputs::default().with_resources(vec![artifact("source.txt")], Vec::new()),
  };
  assert!(matches!(
    validate_registered_resources(&plan, &outside),
    Err(ExecutorError::InvalidCacheConfiguration(message)) if message.contains("not contained")
  ));

  let valid_artifact = CachedArtifact {
    name: "application".to_owned(),
    path: RelativePath::new("dist/app").unwrap(),
    content_type: None,
  };
  validate_resource_contract(&plan, std::slice::from_ref(&valid_artifact), &[]).unwrap();
  validate_materialized_resources(directory.path(), std::slice::from_ref(&valid_artifact), &[]).unwrap();

  let missing_report = CachedReport {
    name: "tests".to_owned(),
    path: RelativePath::new("dist/missing.xml").unwrap(),
    format: "junit".to_owned(),
  };
  assert!(validate_materialized_resources(directory.path(), &[], &[missing_report]).is_err());

  let report_directory = CachedReport {
    name: "tests".to_owned(),
    path: RelativePath::new("dist").unwrap(),
    format: "junit".to_owned(),
  };
  assert!(
    validate_materialized_resources(directory.path(), &[], &[report_directory])
      .unwrap_err()
      .to_string()
      .contains("not a file")
  );

  #[cfg(unix)]
  {
    use std::os::unix::fs::symlink;

    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("escaped"), "outside").unwrap();
    symlink(outside.path().join("escaped"), directory.path().join("dist/escaped")).unwrap();
    let escaped = CachedArtifact {
      name: "escaped".to_owned(),
      path: RelativePath::new("dist/escaped").unwrap(),
      content_type: None,
    };
    assert!(validate_materialized_resources(directory.path(), &[escaped], &[])
      .unwrap_err()
      .to_string()
      .contains("outside"));

    symlink("app", directory.path().join("dist/alias")).unwrap();
    let alias = CachedArtifact {
      name: "alias".to_owned(),
      path: RelativePath::new("dist/alias").unwrap(),
      content_type: None,
    };
    assert!(validate_materialized_resources(directory.path(), &[alias], &[])
      .unwrap_err()
      .to_string()
      .contains("different path"));
  }
}

#[tokio::test]
async fn invalid_publication_metadata_does_not_fail_a_successful_task() {
  let directory = TempDir::new().unwrap();
  let store = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(directory.path().join("cache"))).unwrap());
  let restore = RestoreManager::open(store.layout_root(), BundleLimits::default()).unwrap();
  let cache = ResultCache::new(
    store.clone(),
    restore,
    "test",
    RuntimeIdentity::Native {
      os: PlatformOs::Macos,
      architecture: PlatformArchitecture::Arm64,
      environment: Digest::blake3(b"toolchain"),
    },
  )
  .unwrap();
  let cancel = CancellationToken::new();
  let input_root = cache
    .snapshotter
    .snapshot(directory.path(), &[], &cancel)
    .await
    .unwrap()
    .root;
  let action = Digest::blake3(b"action");
  let state = TaskCacheState::default();
  state
    .set_lookup(
      Some(PublicationIdentity { action, input_root }),
      None,
      CacheOutcome::miss(
        action.to_string(),
        CacheReason::ActionNotFound,
        std::time::Duration::ZERO,
      ),
    )
    .await;
  state
    .record_command(
      0,
      &"x".repeat(MAX_ACTION_RESULT_METADATA_BYTES + 1),
      &CompletionOutputs::default(),
    )
    .await;
  let plan = TaskCachePlan {
    workspace: directory.path().to_path_buf(),
    inputs: Vec::new(),
    outputs: Vec::new(),
    task_definition: Digest::blake3(b"task"),
    environment: Vec::new(),
    plugin_keys: Vec::new(),
    arguments: Vec::new(),
    timeout: None,
    salt: None,
  };
  let output = RuntimeOutput::new(
    Arc::new(Console::default()),
    1,
    Some(ConsoleScopeAllocator::default().scope("task")),
  );

  let (stdout, outputs, outcome) = finalize(&cache, &plan, &state, &output, &cancel, false).await.unwrap();
  assert_eq!(stdout.len(), MAX_ACTION_RESULT_METADATA_BYTES + 1);
  assert_eq!(outcome.status, CacheStatus::Error);
  assert_eq!(outcome.reason, Some(CacheReason::ResultMetadataInvalid));
  assert_eq!(outputs.cache().map(|cache| cache.status), Some(CacheStatus::Error));
  assert!(store.get_action("test", &action).await.unwrap().is_none());
}

#[tokio::test]
async fn lookup_failures_fall_back_while_dry_and_secret_tasks_bypass_storage() {
  let directory = TempDir::new().unwrap();
  let local = LocalCacheStore::open(LocalCacheConfig::new(directory.path().join("local"))).unwrap();
  let restore = RestoreManager::open(local.layout_root(), BundleLimits::default()).unwrap();
  let store = Arc::new(ControlledStore {
    action_read_fails: true,
    ..ControlledStore::default()
  });
  let cache = ResultCache::new(
    store.clone(),
    restore,
    "test",
    RuntimeIdentity::Native {
      os: PlatformOs::Macos,
      architecture: PlatformArchitecture::Arm64,
      environment: Digest::blake3(b"toolchain"),
    },
  )
  .unwrap();
  let plan = TaskCachePlan {
    workspace: directory.path().to_path_buf(),
    inputs: Vec::new(),
    outputs: Vec::new(),
    task_definition: Digest::blake3(b"task"),
    environment: Vec::new(),
    plugin_keys: Vec::new(),
    arguments: Vec::new(),
    timeout: None,
    salt: None,
  };
  let manager = octa_plugin_manager::plugin_manager::PluginManager::new(directory.path());
  let output = RuntimeOutput::new(
    Arc::new(Console::default()),
    1,
    Some(ConsoleScopeAllocator::default().scope("task")),
  );
  let context = |vars| RuntimeContext {
    vars,
    envs: crate::envs::Envs::new(),
    dir: directory.path().to_path_buf(),
    identity_names: HashSet::new(),
    plugin_uses: Arc::new(std::sync::Mutex::new(HashSet::new())),
  };
  let cancel = CancellationToken::new();

  let failed = TaskCacheState::default();
  lookup(
    &cache,
    CacheLookup {
      plan: &plan,
      state: &failed,
      plugin_manager: &manager,
      context: &context(crate::vars::Vars::new()),
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(failed.miss().await.unwrap().2.status, CacheStatus::Error);
  assert_eq!(store.lookups.load(Ordering::Relaxed), 1);

  let dry = TaskCacheState::default();
  lookup(
    &cache,
    CacheLookup {
      plan: &plan,
      state: &dry,
      plugin_manager: &manager,
      context: &context(crate::vars::Vars::new()),
      output: &output,
      cancel: &cancel,
      dry: true,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(dry.miss().await.unwrap().2.status, CacheStatus::Bypassed);

  let secret_values = serde_yml::from_str("TOKEN: { value: hidden, secret: true }").unwrap();
  let secret = TaskCacheState::default();
  lookup(
    &cache,
    CacheLookup {
      plan: &plan,
      state: &secret,
      plugin_manager: &manager,
      context: &context(crate::vars::Vars::with_variables(secret_values)),
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(secret.miss().await.unwrap().2.status, CacheStatus::Bypassed);
  assert_eq!(store.lookups.load(Ordering::Relaxed), 1);

  // An unavailable workspace is a soft cache failure: commands must still be
  // allowed to execute, but no action may be published under a guessed key.
  let missing_plan = TaskCachePlan {
    workspace: directory.path().join("missing-workspace"),
    ..plan.clone()
  };
  let missing = TaskCacheState::default();
  lookup(
    &cache,
    CacheLookup {
      plan: &missing_plan,
      state: &missing,
      plugin_manager: &manager,
      context: &context(crate::vars::Vars::new()),
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  let (_, _, outcome) = missing.miss().await.unwrap();
  assert_eq!(outcome.status, CacheStatus::Error);
  assert_eq!(outcome.reason, Some(CacheReason::InputSnapshotFailed));
  assert_eq!(store.lookups.load(Ordering::Relaxed), 1);

  let descriptor = BlobDescriptor {
    digest: Digest::blake3(b"blob"),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: 4,
    expanded_size_bytes: 4,
    entry_count: 1,
  };
  assert_eq!(
    store
      .find_missing_blobs(std::slice::from_ref(&descriptor))
      .await
      .unwrap(),
    [descriptor]
  );
}

#[tokio::test]
async fn cache_access_modes_gate_storage_reads_and_publication_independently() {
  let directory = TempDir::new().unwrap();
  let manager = octa_plugin_manager::plugin_manager::PluginManager::new(directory.path());
  let context = runtime_context(directory.path());
  let output = runtime_output();
  let cancel = CancellationToken::new();
  let plan = plan(directory.path(), Vec::new(), Vec::new());

  let read_only_store = Arc::new(ControlledStore::default());
  let read_only = runtime_cache(directory.path(), read_only_store.clone()).with_access(CacheMode::ReadOnly);
  let read_only_state = TaskCacheState::default();
  lookup(
    &read_only,
    CacheLookup {
      plan: &plan,
      state: &read_only_state,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  read_only_state
    .record_command(0, "executed", &CompletionOutputs::default())
    .await;
  finalize(&read_only, &plan, &read_only_state, &output, &cancel, false)
    .await
    .unwrap();
  assert_eq!(read_only_store.lookups.load(Ordering::Relaxed), 1);
  assert_eq!(read_only_store.action_writes.load(Ordering::Relaxed), 0);

  let write_only_store = Arc::new(ControlledStore::default());
  let write_only = runtime_cache(directory.path(), write_only_store.clone()).with_access(CacheMode::WriteOnly);
  let write_only_state = TaskCacheState::default();
  lookup(
    &write_only,
    CacheLookup {
      plan: &plan,
      state: &write_only_state,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(
    write_only_state.miss().await.unwrap().2.reason,
    Some(CacheReason::ReadDisabled)
  );
  write_only_state
    .record_command(0, "executed", &CompletionOutputs::default())
    .await;
  finalize(&write_only, &plan, &write_only_state, &output, &cancel, false)
    .await
    .unwrap();
  assert_eq!(write_only_store.lookups.load(Ordering::Relaxed), 0);
  assert_eq!(write_only_store.action_writes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn cache_probe_reports_unusable_blob_references_without_restoring() {
  let directory = TempDir::new().unwrap();
  let manager = octa_plugin_manager::plugin_manager::PluginManager::new(directory.path());
  let context = runtime_context(directory.path());
  let output = runtime_output();
  let cancel = CancellationToken::new();
  let plan = plan(directory.path(), Vec::new(), Vec::new());
  let descriptor = BlobDescriptor {
    digest: Digest::blake3(b"x"),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: 1,
    expanded_size_bytes: 1,
    entry_count: 1,
  };

  let missing_store = Arc::new(ControlledStore::default());
  let missing_cache = runtime_cache(directory.path(), missing_store.clone());
  let action = action_for(&missing_cache, &plan, &manager, &context).await;
  *missing_store.action.lock().await = Some(ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action,
    output_bundle: Some(descriptor.clone()),
    stdout: None,
    task_outputs: BTreeMap::new(),
    artifacts: Vec::new(),
    reports: Vec::new(),
  });
  let missing = TaskCacheState::default();
  missing.set_probe(true).await;
  lookup(
    &missing_cache,
    CacheLookup {
      plan: &plan,
      state: &missing,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(
    missing.miss().await.unwrap().2.reason,
    Some(CacheReason::BlobReadFailed)
  );
  let (_, _, probe_outcome) = finalize(&missing_cache, &plan, &missing, &output, &cancel, false)
    .await
    .unwrap();
  assert_eq!(probe_outcome.reason, Some(CacheReason::BlobReadFailed));
  assert_eq!(missing_store.blob_writes.load(Ordering::Relaxed), 0);
  assert_eq!(missing_store.action_writes.load(Ordering::Relaxed), 0);

  let failing_store = Arc::new(ControlledStore {
    action: Mutex::new(missing_store.action.lock().await.clone()),
    blob_lookup_fails: true,
    ..ControlledStore::default()
  });
  let failing_cache = runtime_cache(directory.path(), failing_store);
  let failing = TaskCacheState::default();
  failing.set_probe(true).await;
  lookup(
    &failing_cache,
    CacheLookup {
      plan: &plan,
      state: &failing,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(
    failing.miss().await.unwrap().2.reason,
    Some(CacheReason::BlobLookupFailed)
  );

  let available_store = Arc::new(ControlledStore {
    action: Mutex::new(missing_store.action.lock().await.clone()),
    blob_missing: false,
    ..ControlledStore::default()
  });
  let available_cache = runtime_cache(directory.path(), available_store);
  let available = TaskCacheState::default();
  available.set_probe(true).await;
  lookup(
    &available_cache,
    CacheLookup {
      plan: &plan,
      state: &available,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(available.hit().await.unwrap().1.status, CacheStatus::Hit);
}

#[tokio::test]
async fn lookup_handles_force_hits_mutation_and_unusable_results() {
  let directory = TempDir::new().unwrap();
  let manager = octa_plugin_manager::plugin_manager::PluginManager::new(directory.path());
  let mut context = runtime_context(directory.path());
  context.envs.insert(&"CONFIGURED".to_owned(), &"value".to_owned());
  let output = runtime_output();
  let cancel = CancellationToken::new();

  let force_store = Arc::new(ControlledStore::default());
  let force_cache = runtime_cache(directory.path(), force_store);
  let mut empty_plan = plan(directory.path(), Vec::new(), Vec::new());
  empty_plan.environment = vec!["OCTA_CACHE_TEST_INTENTIONALLY_UNSET".to_owned()];
  let forced = TaskCacheState::default();
  lookup(
    &force_cache,
    CacheLookup {
      plan: &empty_plan,
      state: &forced,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: true,
    },
  )
  .await
  .unwrap();
  assert_eq!(forced.miss().await.unwrap().2.reason, Some(CacheReason::Force));

  let hit_store = Arc::new(ControlledStore::default());
  let hit_cache = runtime_cache(directory.path(), hit_store.clone());
  let action = action_for(&hit_cache, &empty_plan, &manager, &context).await;
  *hit_store.action.lock().await = Some(ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action,
    output_bundle: None,
    stdout: Some("from cache".to_owned()),
    task_outputs: BTreeMap::new(),
    artifacts: Vec::new(),
    reports: Vec::new(),
  });
  let hit = TaskCacheState::default();
  lookup(
    &hit_cache,
    CacheLookup {
      plan: &empty_plan,
      state: &hit,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert!(hit.miss().await.is_none());
  let (stdout, _, outcome) = finalize(&hit_cache, &empty_plan, &hit, &output, &cancel, false)
    .await
    .unwrap();
  assert_eq!(stdout, "from cache");
  assert_eq!(outcome.status, CacheStatus::Hit);
  assert_eq!(outcome.layer, Some(CacheLayer::Local));

  let recheck_cancel = CancellationToken::new();
  let cancelling_lookup_store = Arc::new(ControlledStore {
    action: Mutex::new(Some(ActionResultV1 {
      result_version: ACTION_RESULT_VERSION_V1,
      action,
      output_bundle: None,
      stdout: None,
      task_outputs: BTreeMap::new(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    })),
    cancel_on_lookup: Some(recheck_cancel.clone()),
    ..ControlledStore::default()
  });
  let cancelling_lookup_cache = runtime_cache(directory.path(), cancelling_lookup_store);
  assert!(matches!(
    lookup(
      &cancelling_lookup_cache,
      CacheLookup {
        plan: &empty_plan,
        state: &TaskCacheState::default(),
        plugin_manager: &manager,
        context: &context,
        output: &output,
        cancel: &recheck_cancel,
        dry: false,
        force: false,
      }
    )
    .await,
    Err(ExecutorError::TaskCancelled(_))
  ));

  let input = directory.path().join("input");
  fs::write(&input, "initial").unwrap();
  let input_plan = plan(directory.path(), vec!["input".to_owned()], Vec::new());
  let mutating_store = Arc::new(ControlledStore {
    mutate_input_on_lookup: Some(input),
    ..ControlledStore::default()
  });
  let mutating_cache = runtime_cache(directory.path(), mutating_store.clone());
  let action = action_for(&mutating_cache, &input_plan, &manager, &context).await;
  *mutating_store.action.lock().await = Some(ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action,
    output_bundle: None,
    stdout: None,
    task_outputs: BTreeMap::new(),
    artifacts: Vec::new(),
    reports: Vec::new(),
  });
  let changed = TaskCacheState::default();
  lookup(
    &mutating_cache,
    CacheLookup {
      plan: &input_plan,
      state: &changed,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  let (publication, _, outcome) = changed.miss().await.unwrap();
  assert!(publication.is_none());
  assert_eq!(outcome.reason, Some(CacheReason::InputsChangedDuringLookup));
  let (_, _, finalized) = finalize(&mutating_cache, &input_plan, &changed, &output, &cancel, false)
    .await
    .unwrap();
  assert_eq!(finalized.reason, Some(CacheReason::InputsChangedDuringLookup));

  let removed_workspace = directory.path().join("removed-during-lookup");
  fs::create_dir(&removed_workspace).unwrap();
  let removed_plan = plan(&removed_workspace, Vec::new(), Vec::new());
  let removed_context = runtime_context(&removed_workspace);
  let removed_store = Arc::new(ControlledStore {
    remove_workspace_on_lookup: Some(removed_workspace.clone()),
    ..ControlledStore::default()
  });
  let removed_cache = runtime_cache(directory.path(), removed_store.clone());
  let removed_action = action_for(&removed_cache, &removed_plan, &manager, &removed_context).await;
  *removed_store.action.lock().await = Some(ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action: removed_action,
    output_bundle: None,
    stdout: None,
    task_outputs: BTreeMap::new(),
    artifacts: Vec::new(),
    reports: Vec::new(),
  });
  let removed = TaskCacheState::default();
  lookup(
    &removed_cache,
    CacheLookup {
      plan: &removed_plan,
      state: &removed,
      plugin_manager: &manager,
      context: &removed_context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(
    removed.miss().await.unwrap().2.reason,
    Some(CacheReason::InputRecheckFailed)
  );

  let descriptor = BlobDescriptor {
    digest: Digest::blake3(b"x"),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: 1,
    expanded_size_bytes: 1,
    entry_count: 1,
  };
  let output_plan = plan(directory.path(), Vec::new(), vec![RelativePath::new("output").unwrap()]);
  let invalid_resource_store = Arc::new(ControlledStore::default());
  let invalid_resource_cache = runtime_cache(directory.path(), invalid_resource_store.clone());
  let invalid_resource_action = action_for(&invalid_resource_cache, &output_plan, &manager, &context).await;
  *invalid_resource_store.action.lock().await = Some(ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action: invalid_resource_action,
    output_bundle: Some(descriptor.clone()),
    stdout: None,
    task_outputs: BTreeMap::new(),
    artifacts: vec![CachedArtifact {
      name: "outside".to_owned(),
      path: RelativePath::new("other/app").unwrap(),
      content_type: None,
    }],
    reports: Vec::new(),
  });
  let invalid_resource = TaskCacheState::default();
  lookup(
    &invalid_resource_cache,
    CacheLookup {
      plan: &output_plan,
      state: &invalid_resource,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(
    invalid_resource.miss().await.unwrap().2.reason,
    Some(CacheReason::ResourceContractInvalid)
  );

  let oversized_bytes = BundleLimits::default().max_encoded_bytes + 1;
  let oversized_store = Arc::new(ControlledStore::default());
  let oversized_cache = runtime_cache(directory.path(), oversized_store.clone());
  let oversized_action = action_for(&oversized_cache, &output_plan, &manager, &context).await;
  *oversized_store.action.lock().await = Some(ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action: oversized_action,
    output_bundle: Some(BlobDescriptor {
      digest: Digest::new(DigestAlgorithm::Blake3, [7; 32], oversized_bytes),
      encoding: BlobEncoding::Identity,
      encoded_size_bytes: oversized_bytes,
      expanded_size_bytes: oversized_bytes,
      entry_count: 1,
    }),
    stdout: None,
    task_outputs: BTreeMap::new(),
    artifacts: Vec::new(),
    reports: Vec::new(),
  });
  let oversized = TaskCacheState::default();
  lookup(
    &oversized_cache,
    CacheLookup {
      plan: &output_plan,
      state: &oversized,
      plugin_manager: &manager,
      context: &context,
      output: &output,
      cancel: &cancel,
      dry: false,
      force: false,
    },
  )
  .await
  .unwrap();
  assert_eq!(
    oversized.miss().await.unwrap().2.reason,
    Some(CacheReason::RestoreFailed)
  );

  for (store, expected) in [
    (Arc::new(ControlledStore::default()), CacheReason::MissingOutputBundle),
    (
      Arc::new(ControlledStore {
        blob_read_fails: true,
        ..ControlledStore::default()
      }),
      CacheReason::BlobReadFailed,
    ),
    (
      Arc::new(ControlledStore {
        blob: Some(Vec::new()),
        ..ControlledStore::default()
      }),
      CacheReason::RestoreFailed,
    ),
  ] {
    let cache = runtime_cache(directory.path(), store.clone());
    let action = action_for(&cache, &output_plan, &manager, &context).await;
    *store.action.lock().await = Some(ActionResultV1 {
      result_version: ACTION_RESULT_VERSION_V1,
      action,
      output_bundle: (expected != CacheReason::MissingOutputBundle).then_some(descriptor.clone()),
      stdout: None,
      task_outputs: BTreeMap::new(),
      artifacts: Vec::new(),
      reports: Vec::new(),
    });
    let state = TaskCacheState::default();
    lookup(
      &cache,
      CacheLookup {
        plan: &output_plan,
        state: &state,
        plugin_manager: &manager,
        context: &context,
        output: &output,
        cancel: &cancel,
        dry: false,
        force: false,
      },
    )
    .await
    .unwrap();
    assert_eq!(state.miss().await.unwrap().2.reason, Some(expected));
  }

  let cancelled = CancellationToken::new();
  cancelled.cancel();
  let state = TaskCacheState::default();
  assert!(matches!(
    lookup(
      &force_cache,
      CacheLookup {
        plan: &empty_plan,
        state: &state,
        plugin_manager: &manager,
        context: &context,
        output: &output,
        cancel: &cancelled,
        dry: false,
        force: false,
      }
    )
    .await,
    Err(ExecutorError::TaskCancelled(_))
  ));

  let restore_cancel = CancellationToken::new();
  let cancelling_store = Arc::new(ControlledStore {
    blob: Some(vec![0]),
    cancel_on_blob_read: Some(restore_cancel.clone()),
    ..ControlledStore::default()
  });
  let cancelling_cache = runtime_cache(directory.path(), cancelling_store.clone());
  let action = action_for(&cancelling_cache, &output_plan, &manager, &context).await;
  *cancelling_store.action.lock().await = Some(ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action,
    output_bundle: Some(descriptor),
    stdout: None,
    task_outputs: BTreeMap::new(),
    artifacts: Vec::new(),
    reports: Vec::new(),
  });
  assert!(matches!(
    lookup(
      &cancelling_cache,
      CacheLookup {
        plan: &output_plan,
        state: &TaskCacheState::default(),
        plugin_manager: &manager,
        context: &context,
        output: &output,
        cancel: &restore_cancel,
        dry: false,
        force: false,
      }
    )
    .await,
    Err(ExecutorError::TaskCancelled(_))
  ));
}

#[tokio::test]
async fn publication_failures_remain_soft_and_never_bind_an_action() {
  let directory = TempDir::new().unwrap();
  let output = runtime_output();
  let cancel = CancellationToken::new();
  let empty_plan = plan(directory.path(), Vec::new(), Vec::new());
  let action = Digest::blake3(b"action");
  let input_root = InputSnapshotter::default()
    .snapshot(directory.path(), &[], &cancel)
    .await
    .unwrap()
    .root;

  let unset = TaskCacheState::default();
  assert!(matches!(
    finalize(
      &runtime_cache(directory.path(), Arc::new(ControlledStore::default())),
      &empty_plan,
      &unset,
      &output,
      &cancel,
      false
    )
    .await,
    Err(ExecutorError::CacheState("finalization ran before lookup"))
  ));

  for (action_write, expected) in [
    (Ok(WriteOutcome::Conflict), CacheReason::NondeterministicResult),
    (Err("offline"), CacheReason::PublicationFailed),
  ] {
    let store = Arc::new(ControlledStore {
      action_write,
      ..ControlledStore::default()
    });
    let cache = runtime_cache(directory.path(), store);
    let state = TaskCacheState::default();
    state
      .set_lookup(
        Some(PublicationIdentity { action, input_root }),
        None,
        CacheOutcome::miss(
          action.to_string(),
          CacheReason::ActionNotFound,
          std::time::Duration::ZERO,
        ),
      )
      .await;
    state
      .record_command(0, "successful task", &CompletionOutputs::default())
      .await;
    let (_, _, outcome) = finalize(&cache, &empty_plan, &state, &output, &cancel, false)
      .await
      .unwrap();
    assert_eq!(outcome.status, CacheStatus::Error);
    assert_eq!(outcome.reason, Some(expected));
  }

  fs::write(directory.path().join("input"), "before").unwrap();
  let input_plan = plan(directory.path(), vec!["input".to_owned()], Vec::new());
  let state = TaskCacheState::default();
  let before = InputSnapshotter::default()
    .snapshot(directory.path(), &input_plan.inputs, &cancel)
    .await
    .unwrap()
    .root;
  state
    .set_lookup(
      Some(PublicationIdentity {
        action,
        input_root: before,
      }),
      None,
      CacheOutcome::miss(
        action.to_string(),
        CacheReason::ActionNotFound,
        std::time::Duration::ZERO,
      ),
    )
    .await;
  fs::write(directory.path().join("input"), "after").unwrap();
  let (_, _, outcome) = finalize(
    &runtime_cache(directory.path(), Arc::new(ControlledStore::default())),
    &input_plan,
    &state,
    &output,
    &cancel,
    false,
  )
  .await
  .unwrap();
  assert_eq!(outcome.reason, Some(CacheReason::InputsChangedBeforePublish));

  // Losing the workspace between execution and the publication recheck is
  // likewise a cache error, never a reason to fail the completed command.
  let removed_workspace = directory.path().join("removed-before-publish");
  fs::create_dir(&removed_workspace).unwrap();
  let removed_plan = plan(&removed_workspace, Vec::new(), Vec::new());
  let removed_root = InputSnapshotter::default()
    .snapshot(&removed_workspace, &[], &cancel)
    .await
    .unwrap()
    .root;
  let removed_state = TaskCacheState::default();
  removed_state
    .set_lookup(
      Some(PublicationIdentity {
        action,
        input_root: removed_root,
      }),
      None,
      CacheOutcome::miss(
        action.to_string(),
        CacheReason::ActionNotFound,
        std::time::Duration::ZERO,
      ),
    )
    .await;
  fs::remove_dir(&removed_workspace).unwrap();
  let (_, _, outcome) = finalize(
    &runtime_cache(directory.path(), Arc::new(ControlledStore::default())),
    &removed_plan,
    &removed_state,
    &output,
    &cancel,
    false,
  )
  .await
  .unwrap();
  assert_eq!(outcome.status, CacheStatus::Error);
  assert_eq!(outcome.reason, Some(CacheReason::InputRecheckFailed));

  let output_plan = plan(
    directory.path(),
    Vec::new(),
    vec![RelativePath::new("missing-output").unwrap()],
  );
  let state = TaskCacheState::default();
  state
    .set_lookup(
      Some(PublicationIdentity { action, input_root }),
      None,
      CacheOutcome::miss(
        action.to_string(),
        CacheReason::ActionNotFound,
        std::time::Duration::ZERO,
      ),
    )
    .await;
  let (_, _, outcome) = finalize(
    &runtime_cache(directory.path(), Arc::new(ControlledStore::default())),
    &output_plan,
    &state,
    &output,
    &cancel,
    false,
  )
  .await
  .unwrap();
  assert_eq!(outcome.reason, Some(CacheReason::OutputCaptureFailed));

  fs::write(directory.path().join("output"), "content").unwrap();
  let output_plan = plan(directory.path(), Vec::new(), vec![RelativePath::new("output").unwrap()]);
  for (blob_write, expected) in [
    (Ok(WriteOutcome::Conflict), CacheReason::BlobConflict),
    (Err("offline"), CacheReason::BlobPublicationFailed),
  ] {
    let store = Arc::new(ControlledStore {
      blob_write,
      ..ControlledStore::default()
    });
    let cache = runtime_cache(directory.path(), store);
    let state = TaskCacheState::default();
    state
      .set_lookup(
        Some(PublicationIdentity { action, input_root }),
        None,
        CacheOutcome::miss(
          action.to_string(),
          CacheReason::ActionNotFound,
          std::time::Duration::ZERO,
        ),
      )
      .await;
    let (_, _, outcome) = finalize(&cache, &output_plan, &state, &output, &cancel, false)
      .await
      .unwrap();
    assert_eq!(outcome.status, CacheStatus::Error);
    assert_eq!(outcome.reason, Some(expected));
  }

  for store in [
    Arc::new(ControlledStore {
      blob_lookup_fails: true,
      ..ControlledStore::default()
    }),
    Arc::new(ControlledStore {
      invalid_missing_response: true,
      ..ControlledStore::default()
    }),
  ] {
    let state = TaskCacheState::default();
    state
      .set_lookup(
        Some(PublicationIdentity { action, input_root }),
        None,
        CacheOutcome::miss(
          action.to_string(),
          CacheReason::ActionNotFound,
          std::time::Duration::ZERO,
        ),
      )
      .await;
    let (_, _, outcome) = finalize(
      &runtime_cache(directory.path(), store),
      &output_plan,
      &state,
      &output,
      &cancel,
      false,
    )
    .await
    .unwrap();
    assert_eq!(outcome.reason, Some(CacheReason::BlobLookupFailed));
  }

  let invalid_resource = TaskCacheState::default();
  invalid_resource
    .set_lookup(
      Some(PublicationIdentity { action, input_root }),
      None,
      CacheOutcome::miss(
        action.to_string(),
        CacheReason::ActionNotFound,
        std::time::Duration::ZERO,
      ),
    )
    .await;
  invalid_resource
    .record_resources(&CompletionOutputs::default().with_resources(vec![artifact("source.txt")], Vec::new()))
    .await;
  let (_, _, outcome) = finalize(
    &runtime_cache(directory.path(), Arc::new(ControlledStore::default())),
    &output_plan,
    &invalid_resource,
    &output,
    &cancel,
    false,
  )
  .await
  .unwrap();
  assert_eq!(outcome.reason, Some(CacheReason::ResourceContractInvalid));

  let present_store = Arc::new(ControlledStore {
    blob_missing: false,
    blob_write: Err("an existing blob must not be uploaded"),
    ..ControlledStore::default()
  });
  let state = TaskCacheState::default();
  state
    .set_lookup(
      Some(PublicationIdentity { action, input_root }),
      None,
      CacheOutcome::miss(
        action.to_string(),
        CacheReason::ActionNotFound,
        std::time::Duration::ZERO,
      ),
    )
    .await;
  let (_, _, outcome) = finalize(
    &runtime_cache(directory.path(), present_store.clone()),
    &output_plan,
    &state,
    &output,
    &cancel,
    false,
  )
  .await
  .unwrap();
  assert_ne!(outcome.status, CacheStatus::Error);
  assert_eq!(present_store.blob_writes.load(Ordering::Relaxed), 0);

  let cancelled = CancellationToken::new();
  cancelled.cancel();
  let state = TaskCacheState::default();
  state
    .set_lookup(
      Some(PublicationIdentity { action, input_root }),
      None,
      CacheOutcome::miss(
        action.to_string(),
        CacheReason::ActionNotFound,
        std::time::Duration::ZERO,
      ),
    )
    .await;
  assert!(matches!(
    finalize(
      &runtime_cache(directory.path(), Arc::new(ControlledStore::default())),
      &empty_plan,
      &state,
      &output,
      &cancelled,
      false
    )
    .await,
    Err(ExecutorError::TaskCancelled(_))
  ));
}
