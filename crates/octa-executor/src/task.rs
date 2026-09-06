use std::{
  collections::{HashMap, HashSet},
  env,
  hash::{Hash, Hasher},
  io,
  path::{Path, PathBuf},
  sync::{Arc, OnceLock},
  time::Duration,
};

use async_trait::async_trait;
use dunce::canonicalize;
use indexmap::IndexMap;
use octa_plugin_manager::plugin_manager::PluginManager;
use serde::Serialize;
use serde_json::Value;
use sled::Db;
use tera::Context;
use tokio::{
  sync::{Mutex, OnceCell},
  time,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, Level};

use octa_dag::Identifiable;
use octa_octafile::{AllowedRun, Timeout};
use octa_output::{Console, ConsoleLevel, ConsoleScope, ConsoleStatus, ConsoleStep};

use crate::{
  console_target::ConsoleTarget,
  envs::{EnvironmentPlan, Envs},
  error::{ExecutorError, ExecutorResult},
  freshness::{FreshnessConfig, FreshnessIdentity, FreshnessOutcome, FreshnessSpec, FreshnessState},
  plugin::{ManagerPluginEvaluator, PluginEvaluator, PluginExecutionContext, PluginInvoker, PluginRequest},
  source_strategy::{SourceStrategyHandle, SourceStrategyRegistry},
  template::{PluginTemplateContext, TemplateRenderer},
  vars::{VariableResolver, Vars},
  watcher::WatchTarget,
};

/// Services and execution settings shared by every node in one executor.
#[derive(Clone)]
pub struct TaskRuntime {
  /// Registry and live connections used to invoke task plugins.
  pub plugin_manager: Arc<PluginManager>,
  /// Per-execution cache shared by nodes in the graph.
  pub cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
  /// Persistent source and output fingerprints.
  pub fingerprint: Arc<Db>,
  /// Destination for command output events.
  pub console: Arc<Console>,
  /// Identifier shared by all events emitted for this execution.
  pub run_id: u64,
  /// Whether plugins should describe work without changing the filesystem.
  pub dry: bool,
  /// Whether freshness checks should be bypassed.
  pub force: bool,
  /// Exit code exposed to commands executing as part of a deferred action.
  pub deferred_exit_code: Option<i32>,
}

/// Result of one DAG node, including whether it performed work or was skipped.
#[derive(Debug, Eq, PartialEq)]
pub struct TaskOutcome {
  output: String,
  status: ConsoleStatus,
}

impl TaskOutcome {
  pub(crate) fn new(output: String, status: ConsoleStatus) -> Self {
    Self { output, status }
  }

  pub fn success(output: impl Into<String>) -> Self {
    Self {
      output: output.into(),
      status: ConsoleStatus::Success,
    }
  }

  pub fn skipped(output: impl Into<String>) -> Self {
    Self {
      output: output.into(),
      status: ConsoleStatus::Skipped,
    }
  }

  pub fn status(&self) -> ConsoleStatus {
    self.status
  }

  pub fn output(&self) -> &str {
    &self.output
  }

  pub fn into_output(self) -> String {
    self.output
  }
}

#[async_trait]
pub trait Executable<T> {
  async fn execute(&self, runtime: TaskRuntime, cancel_token: CancellationToken) -> ExecutorResult<TaskOutcome>;
  async fn set_result(&self, task_name: String, res: String);
  async fn bypass_result(&self, result: HashMap<String, String>);
}

pub use crate::source_strategy::{SourceMethod, SourceStrategy};

/// Output identity attached to one executable DAG node.
///
/// Keeping task and step identity in one value prevents a step from being
/// emitted without its owning task invocation.
#[derive(Clone, Debug)]
pub struct ExecutionBinding {
  scope: ConsoleScope,
  step: Option<ConsoleStep>,
}

impl ExecutionBinding {
  /// Binds a DAG node to task-level lifecycle and diagnostics.
  pub fn for_task(scope: ConsoleScope) -> Self {
    Self { scope, step: None }
  }

  /// Binds a DAG node to an executable step within `scope`.
  ///
  /// # Panics
  ///
  /// Panics when `step` was allocated for a different task scope.
  pub fn for_step(scope: ConsoleScope, step: ConsoleStep) -> Self {
    assert_eq!(step.parent_task_id(), scope.id(), "step must belong to its task scope");
    Self {
      scope,
      step: Some(step),
    }
  }

  pub fn scope(&self) -> &ConsoleScope {
    &self.scope
  }

  pub fn step(&self) -> Option<&ConsoleStep> {
    self.step.as_ref()
  }
}

pub trait TaskItem {
  fn run_mode(&self) -> RunMode;
  fn failfast(&self) -> bool;
  fn requires_concurrency_permit(&self) -> bool;

  fn interactive_session(&self) -> Option<&str> {
    None
  }

  fn requires_runtime_lock(&self) -> bool {
    self.requires_concurrency_permit()
  }

  /// Associates this node with a task invocation.
  ///
  /// Existing implementations may continue to override this task-level hook;
  /// [`Self::execution_binding`] upgrades it to a binding without a step.
  fn output_scope(&self) -> Option<ConsoleScope> {
    None
  }

  /// Associates this node with its task invocation and optional command step.
  fn execution_binding(&self) -> Option<ExecutionBinding> {
    self.output_scope().map(ExecutionBinding::for_task)
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RunMode {
  Always,
  Once,
  Changed,
}

/// Values resolved once and reused throughout one task invocation.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeContext {
  vars: Vars,
  envs: Envs,
  dir: PathBuf,
}

/// Lazily resolved values shared by every graph node in one logical task invocation.
pub(crate) struct InvocationRuntime {
  vars: Vars,
  environment: EnvironmentPlan,
  identity_names: HashSet<String>,
  resolver: Option<Arc<dyn VariableResolver>>,
  context: OnceCell<RuntimeContext>,
}

impl InvocationRuntime {
  pub(crate) fn new(
    vars: Vars,
    environment: EnvironmentPlan,
    identity_names: HashSet<String>,
    resolver: Option<Arc<dyn VariableResolver>>,
  ) -> Self {
    Self {
      vars,
      environment,
      identity_names,
      resolver,
      context: OnceCell::new(),
    }
  }

  pub(crate) fn vars(&self) -> &Vars {
    &self.vars
  }

  pub(crate) fn configured_envs(&self) -> Envs {
    self.environment.configured_envs()
  }

  pub(crate) fn identity_names(&self) -> &HashSet<String> {
    &self.identity_names
  }
}

impl std::fmt::Debug for InvocationRuntime {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("InvocationRuntime")
      .field("initialized", &self.context.get().is_some())
      .field("identity_names", &self.identity_names)
      .finish_non_exhaustive()
  }
}

impl From<AllowedRun> for RunMode {
  fn from(value: AllowedRun) -> Self {
    match value {
      AllowedRun::Once => RunMode::Once,
      AllowedRun::Always => RunMode::Always,
      AllowedRun::Changed => RunMode::Changed,
    }
  }
}

mod config;
pub use config::{CacheItem, TaskConfig, TaskConfigBuilder};
pub(crate) use config::{ConditionRuntime, ConditionState, FreshnessRuntime, NodeAction, PluginInvocation};

/// Represents a single executable task with its configuration and state.
#[derive(Debug, Clone)]
pub struct TaskNode {
  // Task identification
  pub id: String,       // Task uniq id
  pub name: String,     // Task name
  pub dep_name: String, // Name of task in deps

  // Execution configuration
  pub dir: PathBuf,        // Working directory
  pub ignore_errors: bool, // Whether to continue on error
  pub silence: octa_octafile::Silence,
  pub quiet: bool,
  pub raw: bool,
  interactive_session: Option<String>,
  pub failfast: bool, // Cancel parallel work after the first failure

  // Runtime behavior
  pub run_mode: RunMode, // Run mode
  pub vars: Vars,        // Task variables
  pub envs: Envs,        // Task environments
  invocation_runtime: Arc<InvocationRuntime>,
  standalone_freshness: Option<FreshnessConfig>,
  condition_runtime: ConditionRuntime,    // Conditions attached to this graph node
  freshness_runtime: FreshnessRuntime,    // Task-level source and output state
  pub preconditions: Option<Vec<String>>, // Task run preconditions
  pub timeout: Option<Timeout>,           // Maximum task execution time
  execution_binding: Option<ExecutionBinding>,
  prefix_template: Option<String>,

  // State management
  pub deps_res: Arc<Mutex<HashMap<String, String>>>, // Dependencies results
  action: NodeAction,
  plugin: Option<PluginInvocation>,
}

// Implement equality based on task ID
impl Eq for TaskNode {}

impl PartialEq for TaskNode {
  fn eq(&self, other: &Self) -> bool {
    self.id == other.id
  }
}

// Implement hashing based on task ID
impl Hash for TaskNode {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.id.hash(state);
  }
}

mod execution;

#[async_trait]
impl Identifiable for TaskNode {
  fn id(&self) -> String {
    self.id.clone()
  }

  fn name(&self) -> String {
    self.name.clone()
  }

  async fn get_deps_result(&self) -> HashMap<String, String> {
    let res = self.deps_res.lock().await;
    res.clone()
  }

  fn is_internal(&self) -> bool {
    !self.action.is_command()
  }
}

impl TaskItem for TaskNode {
  fn run_mode(&self) -> RunMode {
    self.run_mode.clone()
  }

  fn failfast(&self) -> bool {
    self.failfast
  }

  fn requires_concurrency_permit(&self) -> bool {
    !matches!(self.action, NodeAction::Barrier | NodeAction::FreshnessCommit(_))
  }

  fn interactive_session(&self) -> Option<&str> {
    self.interactive_session.as_deref()
  }

  fn requires_runtime_lock(&self) -> bool {
    self.action.needs_runtime_lock()
  }

  fn output_scope(&self) -> Option<ConsoleScope> {
    self.execution_binding.as_ref().map(|binding| binding.scope().clone())
  }

  fn execution_binding(&self) -> Option<ExecutionBinding> {
    self.execution_binding.clone()
  }
}

#[cfg(test)]
mod task_tests;
