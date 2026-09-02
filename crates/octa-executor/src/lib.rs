mod dotenv;
/// Module for building and managing task execution graphs
pub mod envs;
pub mod error;
pub mod executor;
mod freshness;
mod graph;
mod hash_source;
mod output;
mod path_hash;
mod path_pattern;
mod platform;
mod plugin;
mod source;
mod source_strategy;
pub mod summary;
pub mod task;
mod task_context;
mod task_identity;
mod template;
mod timestamp_source;
mod variable_enum;
pub mod vars;
pub mod watcher;

#[cfg(test)]
mod builder_tests;

use std::{
  collections::{HashMap, HashSet},
  env,
  path::PathBuf,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
  },
};

use envs::Envs;
use octa_plugin_manager::plugin_manager::PluginManager;
use tracing::{debug, info};
use uuid::Uuid;

use error::{ExecutorError, ExecutorResult};
use executor::DeferredAction;
pub use executor::{ExecutionPlan, Executor};
use freshness::{FreshnessConfig, FreshnessIdentity, FreshnessState};
use octa_dag::DAG;
use octa_finder::{FindResult, OctaFinder};
use octa_octafile::{
  AllowedRun, CommandOptions, CommandPayload, ConditionEvaluation, Deps, EnvValue, ExecuteMode, Octafile,
  PluginCommand, SourceStrategies, Task, TaskCommand,
};
use source_strategy::SourceStrategyRegistry;
pub use source_strategy::{SourceMethod, SourceStrategy};
pub use task::TaskNode;
use task::{ConditionRuntime, ConditionState, FreshnessRuntime, NodeAction, PluginInvocation, TaskConfig};
use vars::{VariableResolver, Vars};

// Type aliases for better readability
type DagNode = DAG<TaskNode>;
type ArcNode = Arc<TaskNode>;

/// Conditions inherited by executable nodes in one flattened task invocation.
#[derive(Clone, Default)]
struct ConditionScope {
  guards: Vec<Arc<ConditionState>>,
  per_command: Vec<PluginInvocation>,
  runtime_context: Option<Arc<ConditionState>>,
}

#[derive(Clone)]
struct InvocationContext {
  dep_name: String,
  vars: Option<octa_octafile::Vars>,
  envs: Option<octa_octafile::Envs>,
  conditions: ConditionScope,
  freshness: Option<Arc<FreshnessState>>,
}

impl InvocationContext {
  fn new(
    dep_name: String,
    vars: Option<octa_octafile::Vars>,
    envs: Option<octa_octafile::Envs>,
    conditions: ConditionScope,
  ) -> Self {
    Self {
      dep_name,
      vars,
      envs,
      conditions,
      freshness: None,
    }
  }

  fn with_overrides(&self, vars: Option<octa_octafile::Vars>, envs: Option<octa_octafile::Envs>) -> Self {
    self.child(self.dep_name.clone(), vars, envs)
  }

  fn nested(&self, dep_name: String, vars: Option<octa_octafile::Vars>, envs: Option<octa_octafile::Envs>) -> Self {
    self.child(dep_name, vars, envs)
  }

  fn child(&self, dep_name: String, vars: Option<octa_octafile::Vars>, envs: Option<octa_octafile::Envs>) -> Self {
    Self {
      dep_name,
      vars,
      envs,
      conditions: self.conditions.clone(),
      // Every referenced task owns its freshness boundary. Carrying the caller's
      // decision across this boundary would hide changes to the child's sources,
      // outputs, dotenv files, and dynamic variables.
      freshness: None,
    }
  }
}

#[derive(Clone, Copy)]
enum ConditionPhase {
  Command,
  BeforeDependencies,
  AfterDependencies,
}

impl ConditionPhase {
  fn label(self) -> &'static str {
    match self {
      Self::Command => "Command",
      Self::BeforeDependencies => "Before dependencies",
      Self::AfterDependencies => "After dependencies",
    }
  }
}

struct GateRequest {
  condition: PluginInvocation,
  phase: ConditionPhase,
  context: InvocationContext,
  parents: Vec<ArcNode>,
}

struct InvocationRequest {
  context: InvocationContext,
  entry_parents: Vec<ArcNode>,
  command_condition: Option<PluginCommand>,
}

struct PreparedInvocation {
  context: InvocationContext,
  parents: Vec<ArcNode>,
}

struct CollectedVars {
  runtime: Vars,
  identity_names: HashSet<String>,
}

fn gate_or_parents(gate: Option<&ArcNode>, parents: &[ArcNode]) -> Vec<ArcNode> {
  gate.map(|gate| vec![gate.clone()]).unwrap_or_else(|| parents.to_vec())
}

fn plugin_invocation(command: PluginCommand) -> ExecutorResult<PluginInvocation> {
  let value = serde_json::to_value(command.value)
    .map_err(|error| ExecutorError::ExtraValueConvertError(command.key.clone(), error.to_string()))?;
  Ok(PluginInvocation::new(command.key, value))
}

pub struct TaskGraphBuilder {
  plugin_manager: Arc<PluginManager>,        // Plugin manager for check plugin commands
  finder: Arc<OctaFinder>,                   // Finder for search task in octafile
  dir: PathBuf,                              // Current user directory
  command_args: Vec<String>,                 // Additional task arguments from cli
  variable_overrides: Vec<(String, String)>, // Highest-priority runtime variable overrides
  // Optional input provider shared by the main graph and nested deferred plans.
  variable_resolver: Option<Arc<dyn VariableResolver>>,
  source_strategies: SourceStrategyRegistry,
  os_arch: String,          // Operating system architecture
  os_type: String,          // Operating system type
  defer_order: AtomicUsize, // Declaration order for deferred commands
  // Deferred actions are collected separately and attached to the DAG when the plan is complete.
  deferred: Mutex<HashMap<String, Arc<DeferredAction<TaskNode>>>>,
}

impl TaskGraphBuilder {
  /// Creates a new TaskGraphBuilder instance
  pub fn new(plugin_manager: Arc<PluginManager>) -> ExecutorResult<Self> {
    let current_dir = env::current_dir()?;
    let os_type = platform::normalize_os(&whoami::platform().to_string());
    let os_arch = platform::normalize_architecture(&whoami::cpu_arch().to_string());

    Ok(Self {
      plugin_manager,
      finder: Arc::new(OctaFinder::new()),
      dir: current_dir,
      command_args: vec![],
      variable_overrides: Vec::new(),
      variable_resolver: None,
      source_strategies: SourceStrategyRegistry::default(),
      os_arch,
      os_type,
      defer_order: AtomicUsize::new(0),
      deferred: Mutex::new(HashMap::new()),
    })
  }

  /// Adds ordered runtime variable overrides that take precedence over every configured layer.
  pub fn with_variable_overrides(mut self, variables: Vec<(String, String)>) -> Self {
    self.variable_overrides = variables;
    self
  }

  /// Provides interactive values for variables declared with `required: prompt`.
  pub fn with_variable_resolver(mut self, resolver: Arc<dyn VariableResolver>) -> Self {
    self.variable_resolver = Some(resolver);
    self
  }

  /// Replaces the implementation used for one configured source strategy.
  pub fn with_source_strategy<S>(mut self, method: SourceMethod, strategy: S) -> Self
  where
    S: SourceStrategy + 'static,
  {
    self.source_strategies.register(method, strategy);
    self
  }

  /// Builds a DAG (Directed Acyclic Graph) of tasks from the given Octafile
  ///
  /// # Arguments
  /// * `octafile` - Reference to the Octafile containing task definitions
  /// * `command` - Command to execute
  /// * `run_parallel` - Whether the caller forces parallel execution
  /// * `command_args` - Additional command line arguments
  pub async fn build(
    mut self,
    octafile: Arc<Octafile>,
    command: &str,
    run_parallel: bool,
    command_args: Vec<String>,
  ) -> ExecutorResult<ExecutionPlan<TaskNode>> {
    info!(
      "Building DAG for command {} with provided args {:?}",
      command, command_args
    );

    self.command_args = command_args;
    let mut dag = DAG::new();

    let mut commands = self.find_and_filter_commands(&octafile, command)?;
    commands = self.filter_command_by_platform(commands);
    let platform_skipped = commands.is_empty();
    commands = self.filter_internal_task(commands);

    if commands.is_empty() && !platform_skipped {
      return Err(ExecutorError::CommandNotFound(command.to_string()));
    }

    for cmd in commands {
      self
        .build_invocation(
          &mut dag,
          &cmd,
          InvocationRequest {
            context: InvocationContext::new(cmd.name.clone(), None, None, ConditionScope::default()),
            entry_parents: Vec::new(),
            command_condition: None,
          },
          run_parallel.then_some(true),
        )
        .await?;
    }

    if dag.node_count() == 0 {
      self.create_group_node(&mut dag, Some(AllowedRun::Always), format!("Skipped command {command}"))?;
    }

    self.validate_dag(&dag, command)?;

    let deferred = self
      .deferred
      .into_inner()
      .map_err(|error| ExecutorError::LockError(error.to_string()))?;
    Ok(ExecutionPlan::new(dag, deferred))
  }
}
