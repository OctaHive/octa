use std::{
  collections::HashMap,
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
use tokio::{sync::Mutex, time};
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, Level};

use octa_dag::Identifiable;
use octa_octafile::{AllowedRun, Timeout};
use octa_output::{Console, ConsoleLevel, ConsoleScope, ConsoleStatus};

use crate::{
  console_target::ConsoleTarget,
  envs::Envs,
  error::{ExecutorError, ExecutorResult},
  freshness::{FreshnessConfig, FreshnessIdentity, FreshnessOutcome, FreshnessSpec, FreshnessState, RuntimeContext},
  plugin::{ManagerPluginEvaluator, PluginEvaluator, PluginExecutionContext, PluginInvoker, PluginRequest},
  source_strategy::{SourceStrategyHandle, SourceStrategyRegistry},
  template::{PluginTemplateContext, TemplateRenderer},
  vars::Vars,
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

  /// Associates executable output with its logical task invocation when available.
  fn output_scope(&self) -> Option<ConsoleScope> {
    None
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RunMode {
  Always,
  Once,
  Changed,
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
  standalone_freshness: Option<FreshnessConfig>,
  condition_runtime: ConditionRuntime,    // Conditions attached to this graph node
  freshness_runtime: FreshnessRuntime,    // Task-level source and output state
  pub preconditions: Option<Vec<String>>, // Task run preconditions
  pub timeout: Option<Timeout>,           // Maximum task execution time
  output_scope: Option<ConsoleScope>,
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
    self.output_scope.clone()
  }
}

#[cfg(test)]
mod task_tests;
