//! Freshness evaluation and persistent source fingerprints.
//!
//! Evaluation is split from commit: successful task completion publishes the
//! new fingerprint, while failed or skipped work leaves the previous state
//! intact. This prevents an interrupted run from marking work as current.

use std::{
  collections::HashSet,
  path::PathBuf,
  sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
  },
};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sled::Db;
use tokio_util::sync::CancellationToken;

use crate::{
  envs::Envs,
  error::{ExecutorError, ExecutorResult},
  output::OutputState,
  path_hash, source,
  source_strategy::{SourceMethod, SourceStrategyHandle},
  vars::Vars,
};

const FRESHNESS_KEY_VERSION: &str = "source_v6";

/// Executor-owned inputs that distinguish one freshness decision from another.
#[derive(Clone, Serialize)]
pub(crate) struct FreshnessIdentity {
  task: String,
  invocation: String,
  definition: Value,
  command_args: Vec<String>,
  variable_overrides: Vec<(String, String)>,
  invocation_vars: Option<octa_octafile::Vars>,
  invocation_envs: Option<octa_octafile::Envs>,
  effective_vars: std::collections::HashMap<String, Value>,
  effective_envs: std::collections::HashMap<String, octa_octafile::EnvValue>,
}

impl std::fmt::Debug for FreshnessIdentity {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("FreshnessIdentity")
      .field("task", &self.task)
      .field("invocation", &self.invocation)
      .field("inputs", &"<redacted>")
      .finish()
  }
}

impl FreshnessIdentity {
  pub(crate) fn new(task: impl Into<String>, invocation: impl Into<String>, definition: Value) -> Self {
    Self {
      task: task.into(),
      invocation: invocation.into(),
      definition,
      command_args: Vec::new(),
      variable_overrides: Vec::new(),
      invocation_vars: None,
      invocation_envs: None,
      effective_vars: std::collections::HashMap::new(),
      effective_envs: std::collections::HashMap::new(),
    }
  }

  pub(crate) fn with_invocation_inputs(
    mut self,
    command_args: Vec<String>,
    variable_overrides: Vec<(String, String)>,
    invocation_vars: Option<octa_octafile::Vars>,
    invocation_envs: Option<octa_octafile::Envs>,
  ) -> Self {
    self.command_args = command_args;
    self.variable_overrides = variable_overrides;
    self.invocation_vars = invocation_vars;
    self.invocation_envs = invocation_envs;
    self
  }

  pub(crate) fn with_effective_inputs(
    mut self,
    vars: std::collections::HashMap<String, Value>,
    envs: std::collections::HashMap<String, octa_octafile::EnvValue>,
  ) -> Self {
    self.effective_vars = vars;
    self.effective_envs = envs;
    self
  }
}

/// Hashes invocation metadata without storing variable values in the database key.
fn serialized_digest(identity: &impl Serialize) -> ExecutorResult<String> {
  let value =
    serde_json::to_value(identity).map_err(|error| ExecutorError::FreshnessIdentityError(error.to_string()))?;
  let encoded = serde_json::to_vec(&canonical_value(value))
    .map_err(|error| ExecutorError::FreshnessIdentityError(error.to_string()))?;
  let digest = Sha256::digest(encoded);
  Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Serialize)]
/// Stable portion of the persisted value used to invalidate incompatible state.
struct PersistedIdentity<'a> {
  invocation: &'a FreshnessIdentity,
  sources: &'a Option<Vec<String>>,
  output: &'a Option<Vec<String>>,
  method: &'a str,
  strategy: &'a str,
  compare_output_timestamps: bool,
}

#[derive(Serialize)]
/// Complete payload stored after a successful task invocation.
struct PersistedState<'a> {
  identity: &'a PersistedIdentity<'a>,
  source_fingerprint: &'a [u8],
}

#[derive(Serialize)]
/// Compact database-key identity independent of potentially secret inputs.
struct FreshnessSlot<'a> {
  task: &'a str,
  invocation: &'a str,
}

fn canonical_value(value: serde_json::Value) -> serde_json::Value {
  match value {
    serde_json::Value::Array(values) => serde_json::Value::Array(values.into_iter().map(canonical_value).collect()),
    serde_json::Value::Object(values) => {
      let mut entries: Vec<_> = values.into_iter().collect();
      entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
      serde_json::Value::Object(
        entries
          .into_iter()
          .map(|(key, value)| (key, canonical_value(value)))
          .collect(),
      )
    },
    value => value,
  }
}

/// Filesystem inputs and strategy shared by freshness DAG nodes and watch discovery.
#[derive(Clone)]
pub(crate) struct FreshnessConfig {
  sources: Option<Vec<String>>,
  output: Option<Vec<String>>,
  root: PathBuf,
  method: SourceMethod,
  strategy: SourceStrategyHandle,
}

impl std::fmt::Debug for FreshnessConfig {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("FreshnessConfig")
      .field("sources", &self.sources)
      .field("output", &self.output)
      .field("root", &self.root)
      .field("method", &self.method)
      .field("strategy", &self.strategy.key())
      .finish()
  }
}

impl FreshnessConfig {
  pub(crate) fn new(
    sources: Option<Vec<String>>,
    output: Option<Vec<String>>,
    root: PathBuf,
    method: SourceMethod,
    strategy: SourceStrategyHandle,
  ) -> Self {
    Self {
      sources,
      output,
      root,
      method,
      strategy,
    }
  }

  pub(crate) fn spec(&self, identity: FreshnessIdentity) -> FreshnessSpec {
    FreshnessSpec {
      identity,
      tracked_variables: None,
      config: self.clone(),
    }
  }

  pub(crate) fn watch_target(&self) -> Option<crate::watcher::WatchTarget> {
    self
      .sources
      .clone()
      .map(|sources| crate::watcher::WatchTarget::new(sources, self.root.clone()))
  }

  #[cfg(test)]
  pub(crate) fn method(&self) -> SourceMethod {
    self.method.clone()
  }

  #[cfg(test)]
  pub(crate) fn strategy_key(&self) -> &'static str {
    self.strategy.key()
  }
}

/// A configuration paired with the invocation identity used as its database key.
#[derive(Clone, Debug)]
pub(crate) struct FreshnessSpec {
  identity: FreshnessIdentity,
  tracked_variables: Option<HashSet<String>>,
  config: FreshnessConfig,
}

impl FreshnessSpec {
  pub(crate) fn track_variables(mut self, names: HashSet<String>) -> Self {
    self.tracked_variables = Some(names);
    self
  }

  pub(crate) async fn evaluate(
    &self,
    database: &Db,
    force: bool,
    vars: &Vars,
    envs: &Envs,
    cancel_token: &CancellationToken,
  ) -> ExecutorResult<FreshnessOutcome> {
    let sources = self.config.sources.clone();
    let output = self.config.output.clone();
    let configured_root = self.config.root.clone();
    let compare_output_timestamps = self.config.strategy.compare_output_timestamps();
    let filesystem_cancel_token = cancel_token.clone();
    let (root, sources, output_changed) = tokio::task::spawn_blocking(move || {
      let root = if configured_root.is_absolute() {
        configured_root
      } else {
        std::env::current_dir()?.join(configured_root)
      };
      let root = dunce::canonicalize(root)?;
      let mut source_paths = match &sources {
        Some(patterns) => source::collect(patterns, &root, &filesystem_cancel_token)?,
        None => Vec::new(),
      };
      source_paths.sort_unstable();
      source_paths.dedup();

      let output_changed = match &output {
        Some(patterns) => {
          let state = OutputState::inspect(patterns, &root, &filesystem_cancel_token)?;
          state.missing
            || (compare_output_timestamps
              && state.is_older_than(source::newest_modified(&source_paths, &filesystem_cancel_token)?))
        },
        None => false,
      };

      Ok::<_, ExecutorError>((root, source_paths, output_changed))
    })
    .await??;

    let fingerprint = self.config.strategy.fingerprint(&sources, cancel_token).await?;
    let mut effective_vars = vars.to_merged_hashmap();
    if let Some(names) = &self.tracked_variables {
      effective_vars.retain(|name, _| names.contains(name));
    }
    let invocation = self
      .identity
      .clone()
      .with_effective_inputs(effective_vars, envs.to_merged_hashmap());
    let identity = PersistedIdentity {
      invocation: &invocation,
      sources: &self.config.sources,
      output: &self.config.output,
      method: self.config.method.as_str(),
      strategy: self.config.strategy.key(),
      compare_output_timestamps,
    };
    let state = serialized_digest(&PersistedState {
      identity: &identity,
      source_fingerprint: &fingerprint,
    })?;
    let slot = serialized_digest(&FreshnessSlot {
      task: &self.identity.task,
      invocation: &self.identity.invocation,
    })?;
    let key = format!("{}:{}:{}", FRESHNESS_KEY_VERSION, path_hash::path_identity(&root), slot);
    let state_changed = database.get(key.as_bytes())?.as_deref() != Some(state.as_bytes());

    Ok(FreshnessOutcome {
      should_run: force || state_changed || output_changed,
      key,
      state,
    })
  }

  #[cfg(test)]
  pub(crate) fn method(&self) -> SourceMethod {
    self.config.method()
  }

  #[cfg(test)]
  pub(crate) fn strategy_key(&self) -> &'static str {
    self.config.strategy_key()
  }

  pub(crate) fn watch_target(&self) -> Option<crate::watcher::WatchTarget> {
    self.config.watch_target()
  }
}

#[derive(Clone, Debug)]
pub(crate) struct FreshnessOutcome {
  should_run: bool,
  key: String,
  state: String,
}

impl FreshnessOutcome {
  pub(crate) fn should_run(&self) -> bool {
    self.should_run
  }

  pub(crate) fn commit(&self, database: &Db) -> ExecutorResult<()> {
    database.insert(self.key.as_bytes(), self.state.as_bytes())?;
    Ok(())
  }
}

#[derive(Clone, Debug)]
enum FreshnessDecision {
  Evaluated(FreshnessOutcome),
  Skipped,
}

/// Shares one freshness decision between a task's check, command, and commit nodes.
#[derive(Debug, Default)]
pub(crate) struct FreshnessState {
  decision: OnceLock<FreshnessDecision>,
  // A partial run is not current: skipped command conditions must be evaluated again next time.
  condition_skipped: AtomicBool,
}

impl FreshnessState {
  pub(crate) fn publish(&self, outcome: FreshnessOutcome) -> ExecutorResult<()> {
    self
      .decision
      .set(FreshnessDecision::Evaluated(outcome))
      .map_err(|_| ExecutorError::FreshnessStateAlreadyPublished)
  }

  pub(crate) fn publish_skipped(&self) -> ExecutorResult<()> {
    self
      .decision
      .set(FreshnessDecision::Skipped)
      .map_err(|_| ExecutorError::FreshnessStateAlreadyPublished)
  }

  pub(crate) fn should_run(&self) -> ExecutorResult<bool> {
    self
      .decision
      .get()
      .map(|decision| match decision {
        FreshnessDecision::Evaluated(outcome) => outcome.should_run,
        FreshnessDecision::Skipped => false,
      })
      .ok_or_else(|| ExecutorError::FreshnessStateUnavailable("freshness check has not completed".to_owned()))
  }

  pub(crate) fn mark_condition_skipped(&self) {
    self.condition_skipped.store(true, Ordering::Release);
  }

  pub(crate) fn commit(&self, database: &Db) -> ExecutorResult<()> {
    let decision = self
      .decision
      .get()
      .ok_or_else(|| ExecutorError::FreshnessStateUnavailable("freshness check has not completed".to_owned()))?;
    let FreshnessDecision::Evaluated(outcome) = decision else {
      return Ok(());
    };
    if outcome.should_run && !self.condition_skipped.load(Ordering::Acquire) {
      outcome.commit(database)?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use std::{
    collections::HashMap,
    fs,
    sync::{
      atomic::{AtomicUsize, Ordering},
      Arc,
    },
  };

  use tempfile::TempDir;

  use super::*;

  struct TimestampOutputStrategy {
    comparison_count: Arc<AtomicUsize>,
  }

  #[async_trait::async_trait]
  impl crate::source_strategy::SourceStrategy for TimestampOutputStrategy {
    fn key(&self) -> &'static str {
      "timestamp-output"
    }

    fn compare_output_timestamps(&self) -> bool {
      self.comparison_count.fetch_add(1, Ordering::Relaxed);
      true
    }

    async fn fingerprint(&self, _sources: &[PathBuf], _cancel_token: &CancellationToken) -> ExecutorResult<Vec<u8>> {
      Ok(vec![1])
    }
  }

  fn database() -> Arc<Db> {
    Arc::new(sled::Config::new().temporary(true).open().unwrap())
  }

  fn strategy(method: SourceMethod) -> SourceStrategyHandle {
    crate::source_strategy::SourceStrategyRegistry::default()
      .resolve(&method)
      .unwrap()
  }

  fn identity(definition: serde_json::Value) -> FreshnessIdentity {
    FreshnessIdentity::new("task", "task", definition)
  }

  fn spec(
    definition: serde_json::Value,
    sources: Option<Vec<String>>,
    output: Option<Vec<String>>,
    root: &std::path::Path,
    method: SourceMethod,
  ) -> FreshnessSpec {
    FreshnessConfig::new(sources, output, root.to_path_buf(), method.clone(), strategy(method))
      .spec(identity(definition))
  }

  async fn evaluate(spec: &FreshnessSpec, database: &Db, force: bool) -> FreshnessOutcome {
    spec
      .evaluate(database, force, &Vars::new(), &Envs::new(), &CancellationToken::new())
      .await
      .unwrap()
  }

  fn publish(state: &FreshnessState, outcome: FreshnessOutcome) {
    state.publish(outcome).unwrap();
  }

  #[test]
  fn identity_is_stable_for_unordered_maps_and_changes_with_values() {
    let first = HashMap::from([("B", "two"), ("A", "one")]);
    let second = HashMap::from([("A", "one"), ("B", "two")]);
    let changed = HashMap::from([("A", "one"), ("B", "changed")]);

    assert_eq!(
      serialized_digest(&identity(serde_json::json!(first))).unwrap(),
      serialized_digest(&identity(serde_json::json!(second))).unwrap()
    );
    assert_ne!(
      serialized_digest(&identity(serde_json::json!(first))).unwrap(),
      serialized_digest(&identity(serde_json::json!(changed))).unwrap()
    );
  }

  #[test]
  fn identity_debug_output_redacts_execution_inputs() {
    let secret = "do-not-log-this-value";
    let identity = identity(serde_json::json!({ "token": secret }))
      .with_invocation_inputs(
        vec![secret.to_owned()],
        vec![("TOKEN".to_owned(), secret.to_owned())],
        None,
        None,
      )
      .with_effective_inputs(
        HashMap::from([("TOKEN".to_owned(), serde_json::json!(secret))]),
        HashMap::new(),
      );

    let debug = format!("{identity:?}");

    assert!(!debug.contains(secret));
    assert!(debug.contains("<redacted>"));
  }

  #[tokio::test]
  async fn cancelled_evaluation_stops_before_fingerprinting() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("source.txt"), "source").unwrap();
    let spec = spec(
      serde_json::json!("build"),
      Some(vec!["source.txt".to_owned()]),
      None,
      root.path(),
      SourceMethod::Hash,
    );
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = spec
      .evaluate(&database(), false, &Vars::new(), &Envs::new(), &cancel_token)
      .await;

    assert!(matches!(
      result,
      Err(ExecutorError::IoError(error)) if error.kind() == std::io::ErrorKind::Interrupted
    ));
  }

  #[tokio::test]
  async fn output_configuration_participates_in_the_cache_identity() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, "source").unwrap();
    fs::write(root.path().join("first.out"), "first").unwrap();
    fs::write(root.path().join("second.out"), "second").unwrap();
    let first = spec(
      serde_json::json!("build"),
      Some(vec!["source.txt".to_owned()]),
      Some(vec!["first.out".to_owned()]),
      root.path(),
      SourceMethod::Hash,
    );
    let second = spec(
      serde_json::json!("build"),
      Some(vec!["source.txt".to_owned()]),
      Some(vec!["second.out".to_owned()]),
      root.path(),
      SourceMethod::Hash,
    );
    let database = database();

    let outcome = evaluate(&first, &database, false).await;
    outcome.commit(&database).unwrap();

    assert!(evaluate(&second, &database, false).await.should_run());
  }

  #[tokio::test]
  async fn reverting_configuration_does_not_reuse_historical_state() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("source.txt"), "source").unwrap();
    let first = spec(
      serde_json::json!({ "command": "first" }),
      Some(vec!["source.txt".to_owned()]),
      None,
      root.path(),
      SourceMethod::Hash,
    );
    let second = spec(
      serde_json::json!({ "command": "second" }),
      Some(vec!["source.txt".to_owned()]),
      None,
      root.path(),
      SourceMethod::Hash,
    );
    let database = database();

    evaluate(&first, &database, false).await.commit(&database).unwrap();
    let changed = evaluate(&second, &database, false).await;
    assert!(changed.should_run());
    changed.commit(&database).unwrap();

    assert!(evaluate(&first, &database, false).await.should_run());
    assert_eq!(database.len(), 1);
  }

  #[tokio::test]
  async fn equivalent_root_paths_share_the_same_cache_identity() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("source.txt"), "source").unwrap();
    let direct = spec(
      serde_json::json!("build"),
      Some(vec!["source.txt".to_owned()]),
      None,
      root.path(),
      SourceMethod::Hash,
    );
    let equivalent = spec(
      serde_json::json!("build"),
      Some(vec!["source.txt".to_owned()]),
      None,
      &root.path().join("."),
      SourceMethod::Hash,
    );
    let database = database();

    evaluate(&direct, &database, false).await.commit(&database).unwrap();

    assert!(!evaluate(&equivalent, &database, false).await.should_run());
  }

  #[tokio::test]
  async fn missing_output_keeps_a_task_stale_after_its_sources_are_committed() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, "source").unwrap();
    let spec = spec(
      serde_json::json!("build"),
      Some(vec![source.to_string_lossy().into_owned()]),
      Some(vec!["app".to_owned()]),
      root.path(),
      SourceMethod::Hash,
    );
    let database = database();

    let first = evaluate(&spec, &database, false).await;
    assert!(first.should_run);
    let state = FreshnessState::default();
    publish(&state, first);
    state.commit(&database).unwrap();

    assert!(evaluate(&spec, &database, false).await.should_run);
    fs::write(root.path().join("app"), "output").unwrap();
    assert!(!evaluate(&spec, &database, false).await.should_run);
  }

  #[tokio::test]
  async fn a_completed_empty_scope_marks_sources_as_current() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, "source").unwrap();
    let spec = spec(
      serde_json::json!("build"),
      Some(vec![source.to_string_lossy().into_owned()]),
      None,
      root.path(),
      SourceMethod::Hash,
    );
    let database = database();

    let state = FreshnessState::default();
    publish(&state, evaluate(&spec, &database, false).await);
    state.commit(&database).unwrap();

    assert!(!evaluate(&spec, &database, false).await.should_run);
  }

  #[tokio::test]
  async fn force_marks_an_up_to_date_task_for_execution() {
    let root = TempDir::new().unwrap();
    let spec = spec(
      serde_json::json!("build"),
      None,
      Some(Vec::new()),
      root.path(),
      SourceMethod::Hash,
    );
    let database = database();
    let state = FreshnessState::default();
    publish(&state, evaluate(&spec, &database, false).await);
    state.commit(&database).unwrap();

    assert!(evaluate(&spec, &database, true).await.should_run);
  }

  #[tokio::test]
  async fn timestamp_method_rejects_outputs_older_than_sources() {
    let root = TempDir::new().unwrap();
    let output = root.path().join("app");
    let source = root.path().join("source.txt");
    fs::write(&output, "output").unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
    fs::write(&source, "source").unwrap();
    let spec = spec(
      serde_json::json!("build"),
      Some(vec![source.to_string_lossy().into_owned()]),
      Some(vec!["app".to_owned()]),
      root.path(),
      SourceMethod::Timestamp,
    );
    let database = database();

    let first = evaluate(&spec, &database, false).await;
    let state = FreshnessState::default();
    publish(&state, first);
    state.commit(&database).unwrap();

    assert!(evaluate(&spec, &database, false).await.should_run);
  }

  #[tokio::test]
  async fn custom_strategy_controls_output_timestamp_comparison() {
    let root = TempDir::new().unwrap();
    let output = root.path().join("app");
    let source = root.path().join("source.txt");
    fs::write(&output, "output").unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
    fs::write(&source, "source").unwrap();
    let comparison_count = Arc::new(AtomicUsize::new(0));
    let spec = FreshnessConfig::new(
      Some(vec!["source.txt".to_owned()]),
      Some(vec!["app".to_owned()]),
      root.path().to_path_buf(),
      SourceMethod::custom("timestamp-output"),
      SourceStrategyHandle::new(TimestampOutputStrategy {
        comparison_count: Arc::clone(&comparison_count),
      }),
    )
    .spec(identity(serde_json::json!("build")));
    let database = database();

    let first = evaluate(&spec, &database, false).await;
    assert_eq!(comparison_count.load(Ordering::Relaxed), 1);
    first.commit(&database).unwrap();

    assert!(evaluate(&spec, &database, false).await.should_run());
    assert_eq!(comparison_count.load(Ordering::Relaxed), 2);
  }

  #[test]
  fn skipped_and_unpublished_states_are_handled_explicitly() {
    let unpublished = FreshnessState::default();
    assert!(matches!(
      unpublished.should_run(),
      Err(ExecutorError::FreshnessStateUnavailable(_))
    ));
    assert!(matches!(
      unpublished.commit(&database()),
      Err(ExecutorError::FreshnessStateUnavailable(_))
    ));

    let skipped = FreshnessState::default();
    skipped.publish_skipped().unwrap();
    assert!(!skipped.should_run().unwrap());
    skipped.commit(&database()).unwrap();
  }

  #[tokio::test]
  async fn skipped_command_prevents_fingerprint_commit() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, "source").unwrap();
    let spec = spec(
      serde_json::json!("build"),
      Some(vec![source.to_string_lossy().into_owned()]),
      None,
      root.path(),
      SourceMethod::Hash,
    );
    let database = database();
    let state = FreshnessState::default();

    publish(&state, evaluate(&spec, &database, false).await);
    state.mark_condition_skipped();
    state.commit(&database).unwrap();

    assert!(evaluate(&spec, &database, false).await.should_run());
  }

  #[test]
  fn decision_can_only_be_published_once() {
    let state = FreshnessState::default();

    state.publish_skipped().unwrap();

    assert!(matches!(
      state.publish_skipped(),
      Err(ExecutorError::FreshnessStateAlreadyPublished)
    ));
  }
}
