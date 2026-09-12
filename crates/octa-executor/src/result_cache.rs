//! Task-level orchestration for persistent action-result caching.
//!
//! The filesystem/cache crate owns safe snapshots, bundles, restoration, and
//! storage. This module owns executor semantics: when an action may be reused,
//! which resolved values enter its identity, and when a successful task is
//! eligible for publication. Keeping that boundary here prevents Octafile or
//! storage implementations from learning about DAG nodes and plugin results.

mod identity;
mod lookup;
mod publication;
mod replay;

pub(crate) use lookup::{lookup, CacheLookup};
pub(crate) use publication::finalize;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use octa_cache::{
  validate_namespace, BundleEncoding, BundleLimits, CacheError, CacheStore, InputSnapshotter, RestoreManager,
  SnapshotOptions,
};
use octa_cache_protocol::{CacheMode, Digest, RelativePath, RuntimeIdentity};
use octa_output::CacheReason;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{error::ExecutorResult, execution_result::CacheOutcome, structured_output::CompletionOutputs};
use identity::digest_json;
use replay::aggregate;

/// Compatibility epoch for executor behavior that can change cached results.
///
/// This is deliberately independent from the crate release version: a CLI or
/// diagnostics-only release must not invalidate every otherwise identical
/// action. Increment it only when executor semantics change incompatibly.
pub(crate) const EXECUTOR_CACHE_SEMANTICS_V1: u16 = 1;

/// Application-owned cache services shared by every execution engine task.
///
/// There is intentionally no executor-specific storage trait. Local and remote
/// stores implement [`CacheStore`]; all higher-level policy remains concrete so
/// a second backend cannot force duplicated orchestration abstractions.
#[derive(Clone)]
pub struct ResultCache {
  store: Arc<dyn CacheStore>,
  snapshotter: InputSnapshotter,
  restore: RestoreManager,
  namespace: Arc<str>,
  runtime: RuntimeIdentity,
  bundle_encoding: BundleEncoding,
  bundle_limits: BundleLimits,
  access: CacheMode,
}

impl ResultCache {
  /// Builds one cache service from application-selected storage and runtime identity.
  pub fn new(
    store: Arc<dyn CacheStore>,
    restore: RestoreManager,
    namespace: impl Into<Arc<str>>,
    runtime: RuntimeIdentity,
  ) -> Result<Self, CacheError> {
    let namespace = namespace.into();
    validate_namespace(&namespace)?;
    Ok(Self {
      store,
      snapshotter: InputSnapshotter::default(),
      restore,
      namespace,
      runtime,
      bundle_encoding: BundleEncoding::default(),
      bundle_limits: BundleLimits::default(),
      access: CacheMode::ReadWrite,
    })
  }

  /// Applies runtime read and publication policy to this cache service.
  pub fn with_access(mut self, access: CacheMode) -> Self {
    self.access = access;
    self
  }

  /// Replaces default hashing bounds while retaining one shared scheduler.
  pub fn with_snapshot_options(mut self, options: SnapshotOptions) -> Result<Self, octa_cache::CacheError> {
    self.snapshotter = InputSnapshotter::new(options)?;
    Ok(self)
  }

  /// Replaces output encoding and safety limits for capture and restore.
  pub fn with_bundle_options(
    mut self,
    encoding: BundleEncoding,
    limits: BundleLimits,
  ) -> Result<Self, octa_cache::CacheError> {
    self.bundle_encoding = encoding.validate()?;
    self.bundle_limits = limits.validate()?;
    Ok(self)
  }

  pub(crate) fn snapshotter(&self) -> InputSnapshotter {
    self.snapshotter.clone()
  }
}

impl std::fmt::Debug for ResultCache {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("ResultCache")
      .field("namespace", &self.namespace)
      .field("runtime", &self.runtime)
      .field("bundle_encoding", &self.bundle_encoding)
      .field("bundle_limits", &self.bundle_limits)
      .field("access", &self.access)
      .finish_non_exhaustive()
  }
}

/// Immutable cache inputs compiled for one logical task invocation.
#[derive(Clone, Debug)]
pub(crate) struct TaskCachePlan {
  pub(crate) workspace: PathBuf,
  pub(crate) inputs: Vec<String>,
  pub(crate) outputs: Vec<RelativePath>,
  pub(crate) task_definition: Digest,
  pub(crate) environment: Vec<String>,
  pub(crate) plugin_keys: Vec<String>,
  pub(crate) arguments: Vec<String>,
  pub(crate) timeout: Option<std::time::Duration>,
  pub(crate) salt: Option<String>,
}

impl TaskCachePlan {
  /// Hashes a parser-independent semantic task representation.
  pub(crate) fn definition_digest(value: &Value) -> ExecutorResult<Digest> {
    digest_json(value)
  }
}

/// Shared state written by lookup/command/resource nodes and consumed by finalize.
#[derive(Debug, Default)]
pub(crate) struct TaskCacheState {
  inner: Mutex<CacheState>,
}

#[derive(Debug, Default)]
struct CacheState {
  /// Diagnostic probes compute lookup state but never execute or publish work.
  probe: bool,
  lookup: Option<LookupState>,
  captures: BTreeMap<usize, CapturedResult>,
  resources: Option<CompletionOutputs>,
  /// A non-fatal lifecycle failure that makes publication unsafe.
  publication_blocked: Option<CacheReason>,
}

#[derive(Debug)]
struct LookupState {
  /// Stable pre-execution identity retained only when publication is safe.
  publication: Option<PublicationIdentity>,
  hit: Option<CapturedResult>,
  outcome: CacheOutcome,
}

#[derive(Clone, Copy, Debug)]
struct PublicationIdentity {
  action: Digest,
  input_root: Digest,
}

#[derive(Clone, Debug)]
struct CapturedResult {
  stdout: String,
  outputs: CompletionOutputs,
}

impl TaskCacheState {
  pub(crate) async fn set_probe(&self, probe: bool) {
    self.inner.lock().await.probe = probe;
  }

  pub(crate) async fn is_probe(&self) -> bool {
    self.inner.lock().await.probe
  }

  async fn set_lookup(
    &self,
    publication: Option<PublicationIdentity>,
    hit: Option<CapturedResult>,
    outcome: CacheOutcome,
  ) {
    self.inner.lock().await.lookup = Some(LookupState {
      publication,
      hit,
      outcome,
    });
  }

  pub(crate) async fn should_skip_body(&self) -> bool {
    let state = self.inner.lock().await;
    state.probe || state.lookup.as_ref().is_some_and(|lookup| lookup.hit.is_some())
  }

  pub(crate) async fn record_command(&self, position: usize, stdout: &str, outputs: &CompletionOutputs) {
    self.inner.lock().await.captures.insert(
      position,
      CapturedResult {
        stdout: stdout.to_owned(),
        outputs: outputs.clone(),
      },
    );
  }

  pub(crate) async fn record_resources(&self, outputs: &CompletionOutputs) {
    self.inner.lock().await.resources = Some(outputs.clone());
  }

  /// Prevents publication without changing the main task conclusion.
  pub(crate) async fn block_publication(&self, reason: CacheReason) {
    self.inner.lock().await.publication_blocked.get_or_insert(reason);
  }

  async fn miss(&self) -> Option<(Option<PublicationIdentity>, CapturedResult, CacheOutcome)> {
    let state = self.inner.lock().await;
    let lookup = state.lookup.as_ref()?;
    if lookup.hit.is_some() {
      return None;
    }
    let mut outcome = lookup.outcome.clone();
    if let Some(reason) = state.publication_blocked {
      outcome.reason = Some(reason);
    }
    Some((
      state
        .publication_blocked
        .is_none()
        .then_some(lookup.publication)
        .flatten(),
      aggregate(&state.captures, state.resources.as_ref()),
      outcome,
    ))
  }

  async fn hit(&self) -> Option<(CapturedResult, CacheOutcome)> {
    let state = self.inner.lock().await;
    let lookup = state.lookup.as_ref()?;
    Some((lookup.hit.clone()?, lookup.outcome.clone()))
  }
}

#[cfg(test)]
#[path = "result_cache_tests.rs"]
mod tests;
