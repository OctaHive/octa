//! Compiles Octafile task declarations into executable plans.

mod graph;
mod platform;
mod task_context;

#[cfg(test)]
mod tests;

use std::{
  collections::{HashMap, HashSet},
  env,
  path::PathBuf,
  sync::Arc,
};

use crate::envs::EnvironmentPlan;
use octa_plugin_manager::plugin_manager::PluginManager;
use tracing::debug;
use uuid::Uuid;

use crate::error::{ExecutorError, ExecutorResult};
#[cfg(test)]
use crate::executor::Executor;
use crate::executor::{DeferredAction, ExecutionPlan};
use crate::freshness::{FreshnessConfig, FreshnessIdentity, FreshnessState};
#[cfg(test)]
use crate::source_strategy::SourceStrategy;
use crate::source_strategy::{SourceMethod, SourceStrategyRegistry};
use crate::task::{self, TaskNode};
use crate::task::{
  ConditionRuntime, ConditionState, ExecutionBinding, FreshnessRuntime, NodeAction, PluginInvocation, TaskConfig,
};
use crate::task_identity;
use crate::vars::{VariableResolver, Vars};
use octa_dag::DAG;
use octa_finder::{FindResult, OctaFinder};
use octa_octafile::{
  AllowedRun, CommandOptions, CommandPayload, ConditionEvaluation, Deps, ExecuteMode, Octafile, PluginCommand,
  SourceStrategies, Task, TaskCommand, TaskOutputMode,
};
use octa_output::{ConsoleScope, ConsoleScopeAllocator, RenderMode};

// Planner signatures refer to these types frequently; aliases keep graph
// expansion focused on relationships instead of wrapper syntax.
type DagNode = DAG<TaskNode>;
type ArcNode = Arc<TaskNode>;

/// Conditions inherited by executable nodes in one flattened task invocation.
#[derive(Clone, Default)]
struct ConditionScope {
  guards: Vec<Arc<ConditionState>>,
  per_command: Vec<PluginInvocation>,
}

#[derive(Clone)]
/// Values inherited while recursively expanding one task invocation.
struct InvocationContext {
  dep_name: String,
  vars: Option<octa_octafile::Vars>,
  envs: Option<octa_octafile::Envs>,
  conditions: ConditionScope,
  runtime: Option<Arc<task::InvocationRuntime>>,
  freshness: Option<Arc<FreshnessState>>,
  parent_task_id: Option<u64>,
  output_scope: Option<ConsoleScope>,
  interactive_session: Option<String>,
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
      runtime: None,
      freshness: None,
      parent_task_id: None,
      output_scope: None,
      interactive_session: None,
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
      runtime: None,
      // Every referenced task owns its freshness boundary. Carrying the caller's
      // decision across this boundary would hide changes to the child's sources,
      // outputs, dotenv files, and dynamic variables.
      freshness: None,
      parent_task_id: self.output_scope.as_ref().map(ConsoleScope::id),
      output_scope: None,
      interactive_session: self.interactive_session.clone(),
    }
  }
}

#[derive(Clone, Copy)]
/// Point in the invocation DAG at which a condition is evaluated.
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

/// Inputs needed to insert a condition gate into the graph.
struct GateRequest {
  condition: PluginInvocation,
  phase: ConditionPhase,
  context: InvocationContext,
  parents: Vec<ArcNode>,
}

/// Inputs that distinguish one recursive task invocation from its parent.
struct InvocationRequest {
  context: InvocationContext,
  entry_parents: Vec<ArcNode>,
  command_condition: Option<PluginCommand>,
}

/// Invocation context after task-local variables, conditions, and freshness are resolved.
struct PreparedInvocation {
  context: InvocationContext,
  parents: Vec<ArcNode>,
}

/// Resolved runtime values plus names that participate in freshness identity.
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

/// Stateful compiler for one requested task and all recursively referenced tasks.
pub(crate) struct TaskGraphBuilder {
  plugin_manager: Arc<PluginManager>,
  finder: Arc<OctaFinder>,
  /// Workspace used for task lookup and relative working directories.
  dir: PathBuf,
  /// Arguments exposed to task templates and commands.
  command_args: Vec<String>,
  /// Highest-priority runtime variable overrides in declaration order.
  variable_overrides: Vec<(String, String)>,
  // Optional input provider shared by the main graph and nested deferred plans.
  variable_resolver: Option<Arc<dyn VariableResolver>>,
  source_strategies: SourceStrategyRegistry,
  scope_allocator: Arc<ConsoleScopeAllocator>,
  force_quiet: bool,
  force_silence: Option<octa_octafile::Silence>,
  force_raw: bool,
  scopes: Vec<ConsoleScope>,
  os_arch: String,
  os_type: String,
  /// Monotonic declaration order used to reverse deferred execution.
  defer_order: usize,
  // Deferred actions are collected separately and attached to the DAG when the plan is complete.
  deferred: HashMap<String, Arc<DeferredAction<TaskNode>>>,
}

impl TaskGraphBuilder {
  /// Creates a new TaskGraphBuilder instance
  pub(crate) fn new(plugin_manager: Arc<PluginManager>) -> ExecutorResult<Self> {
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
      scope_allocator: Arc::new(ConsoleScopeAllocator::default()),
      force_quiet: false,
      force_silence: None,
      force_raw: false,
      scopes: Vec::new(),
      os_arch,
      os_type,
      defer_order: 0,
      deferred: HashMap::new(),
    })
  }

  /// Adds ordered runtime variable overrides that take precedence over every configured layer.
  pub(crate) fn with_variable_overrides(mut self, variables: Vec<(String, String)>) -> Self {
    self.variable_overrides = variables;
    self
  }

  /// Sets the workspace used to resolve task working directories.
  pub(crate) fn with_working_directory(mut self, directory: PathBuf) -> Self {
    self.dir = if directory.is_absolute() {
      directory
    } else {
      self.dir.join(directory)
    };
    self
  }

  /// Provides interactive values for variables declared with `required: prompt`.
  pub(crate) fn with_variable_resolver(mut self, resolver: Arc<dyn VariableResolver>) -> Self {
    self.variable_resolver = Some(resolver);
    self
  }

  /// Shares ordered output-scope allocation with every plan built for one CLI run.
  pub(crate) fn with_scope_allocator(mut self, allocator: Arc<ConsoleScopeAllocator>) -> Self {
    self.scope_allocator = allocator;
    self
  }

  /// Applies process-wide output visibility and raw transport overrides.
  pub(crate) fn with_output_overrides(
    mut self,
    quiet: bool,
    silence: Option<octa_octafile::Silence>,
    raw: bool,
  ) -> Self {
    self.force_quiet = quiet;
    self.force_silence = silence;
    self.force_raw = raw;
    self
  }

  /// Replaces the implementation used for one configured source strategy.
  #[cfg(test)]
  pub(crate) fn with_source_strategy<S>(mut self, method: SourceMethod, strategy: S) -> Self
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
  pub(crate) async fn build(
    mut self,
    octafile: Arc<Octafile>,
    command: &str,
    run_parallel: bool,
    command_args: Vec<String>,
  ) -> ExecutorResult<ExecutionPlan<TaskNode>> {
    debug!(
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

    Ok(ExecutionPlan::new(dag, self.deferred, self.scopes))
  }
}
