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
use tracing::{debug, enabled, error, info, Level};

use octa_dag::Identifiable;
use octa_octafile::{AllowedRun, Timeout};

use crate::{
  envs::Envs,
  error::{ExecutorError, ExecutorResult},
  freshness::{FreshnessConfig, FreshnessIdentity, FreshnessOutcome, FreshnessSpec, FreshnessState, RuntimeContext},
  plugin::{ManagerPluginEvaluator, PluginEvaluator, PluginExecutionContext, PluginInvoker, PluginRequest},
  source_strategy::{SourceStrategyHandle, SourceStrategyRegistry},
  template::{PluginTemplateContext, TemplateRenderer},
  vars::Vars,
  watcher::WatchTarget,
};

/// Core traits and types
#[async_trait]
pub trait Executable<T> {
  async fn execute(
    &self,
    plugin_manager: Arc<PluginManager>,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    fingerprint: Arc<Db>,
    dry: bool,
    force: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<String>;
  async fn set_result(&self, task_name: String, res: String);
  async fn bypass_result(&self, result: HashMap<String, String>);
}

pub use crate::source_strategy::{SourceMethod, SourceStrategy};

pub trait TaskItem {
  fn run_mode(&self) -> RunMode;
  fn failfast(&self) -> bool;
  fn requires_concurrency_permit(&self) -> bool;
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
  pub silent: bool,        // Should task print to stdout or stderr
  pub failfast: bool,      // Cancel parallel work after the first failure

  // Runtime behavior
  pub run_mode: RunMode, // Run mode
  pub vars: Vars,        // Task variables
  pub envs: Envs,        // Task environments
  standalone_freshness: Option<FreshnessConfig>,
  condition_runtime: ConditionRuntime,    // Conditions attached to this graph node
  freshness_runtime: FreshnessRuntime,    // Task-level source and output state
  pub preconditions: Option<Vec<String>>, // Task run preconditions
  pub timeout: Option<Timeout>,           // Maximum task execution time

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
}

#[cfg(test)]
mod task_tests;
