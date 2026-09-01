mod dotenv;
/// Module for building and managing task execution graphs
pub mod envs;
pub mod error;
pub mod executor;
mod function;
mod hash_source;
mod source;
pub mod summary;
pub mod task;
mod timestamp_source;
pub mod vars;
pub mod watcher;

use std::{
  collections::HashMap,
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
use octa_dag::DAG;
use octa_finder::{FindResult, OctaFinder};
use octa_octafile::{
  AllowedRun, CommandOptions, CommandPayload, ConditionEvaluation, Deps, EnvValue, ExecuteMode, Octafile,
  PluginCommand, Task, TaskCommand,
};
pub use task::TaskNode;
use task::{ConditionRuntime, ConditionState, NodeKind, PluginInvocation, TaskConfig};
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
    }
  }

  fn with_overrides(&self, vars: Option<octa_octafile::Vars>, envs: Option<octa_octafile::Envs>) -> Self {
    Self::new(self.dep_name.clone(), vars, envs, self.conditions.clone())
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

fn gate_or_parents(gate: Option<&ArcNode>, parents: &[ArcNode]) -> Vec<ArcNode> {
  gate.map(|gate| vec![gate.clone()]).unwrap_or_else(|| parents.to_vec())
}

fn plugin_invocation(command: PluginCommand) -> ExecutorResult<PluginInvocation> {
  let value = serde_json::to_value(command.value)
    .map_err(|error| ExecutorError::ExtraValueConvertError(command.key.clone(), error.to_string()))?;
  Ok(PluginInvocation::new(command.key, value))
}

fn normalize_platform(value: &str) -> String {
  value
    .chars()
    .filter(|character| !character.is_whitespace())
    .flat_map(char::to_lowercase)
    .collect()
}

fn normalize_os(value: &str) -> String {
  match normalize_platform(value).as_str() {
    "darwin" | "osx" => "macos".to_string(),
    os => os.to_string(),
  }
}

fn normalize_architecture(value: &str) -> String {
  match normalize_platform(value).as_str() {
    "amd64" | "x64" | "x86-64" => "x86_64".to_string(),
    "aarch64" => "arm64".to_string(),
    architecture => architecture.to_string(),
  }
}

fn matches_platform(selector: &str, os_type: &str, os_arch: &str) -> bool {
  if let Some((platform, architecture)) = selector.split_once('/') {
    return normalize_os(platform) == os_type && normalize_architecture(architecture) == os_arch;
  }

  normalize_os(selector) == os_type || normalize_architecture(selector) == os_arch
}

pub struct TaskGraphBuilder {
  plugin_manager: Arc<PluginManager>,        // Plugin manager for check plugin commands
  finder: Arc<OctaFinder>,                   // Finder for search task in octafile
  dir: PathBuf,                              // Current user directory
  command_args: Vec<String>,                 // Additional task arguments from cli
  variable_overrides: Vec<(String, String)>, // Highest-priority runtime variable overrides
  // Optional input provider shared by the main graph and nested deferred plans.
  variable_resolver: Option<Arc<dyn VariableResolver>>,
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
    let os_type = normalize_os(&whoami::platform().to_string());
    let os_arch = normalize_architecture(&whoami::cpu_arch().to_string());

    Ok(Self {
      plugin_manager,
      finder: Arc::new(OctaFinder::new()),
      dir: current_dir,
      command_args: vec![],
      variable_overrides: Vec::new(),
      variable_resolver: None,
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

  /// Builds a DAG (Directed Acyclic Graph) of tasks from the given Octafile
  ///
  /// # Arguments
  /// * `octafile` - Reference to the Octafile containing task definitions
  /// * `command` - Command to execute
  /// * `run_parallel` - Whether tasks can run in parallel
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
          Some(run_parallel),
        )
        .await?;
    }

    if dag.node_count() == 0 {
      self.create_group_node(&mut dag, Some(AllowedRun::Always), format!("Skipped command {command}"))?;
    }

    self.validate_dag(&dag, command)?;

    Ok(ExecutionPlan::new(dag, self.deferred.into_inner().unwrap()))
  }

  async fn build_invocation(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    request: InvocationRequest,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<Option<ArcNode>> {
    Box::pin(self._build_invocation(dag, command, request, run_parallel)).await
  }

  async fn _build_invocation(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    request: InvocationRequest,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<Option<ArcNode>> {
    let prepared = self.prepare_invocation(dag, command, request).await?;
    self
      .build_task_body(dag, command, prepared.context, prepared.parents, run_parallel)
      .await
  }

  async fn build_task_body(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    context: InvocationContext,
    parents: Vec<ArcNode>,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<Option<ArcNode>> {
    let Some(commands) = &command.task.cmds else {
      let task = self.create_task_node(dag, command, &context, None)?;
      Self::connect_parents(dag, &parents, &task)?;
      return Ok(Some(task));
    };

    let run_parallel = run_parallel.unwrap_or(matches!(command.task.execute_mode, Some(ExecuteMode::Parallel)));
    let mut sequential_parent = None;
    let mut parallel_terminals = Vec::new();
    let mut deferred_nodes = Vec::new();

    for command_item in commands {
      if !self.matches_platforms(command_item.options.platforms.as_deref()) {
        continue;
      }

      let entries = if run_parallel {
        parents.clone()
      } else {
        sequential_parent
          .clone()
          .map_or_else(|| parents.clone(), |parent| vec![parent])
      };

      if command_item.options.deferred {
        let registered_after = entries.iter().map(|task| task.id.clone()).collect();
        deferred_nodes.push(
          self
            .create_deferred_node(dag, command, command_item, context.clone(), registered_after)
            .await?,
        );
        continue;
      }

      let mut terminals = match &command_item.payload {
        CommandPayload::Task(complex) => {
          let mut referenced_tasks = self.find_and_filter_commands(&command.octafile, &complex.task)?;
          referenced_tasks = self.filter_command_by_platform(referenced_tasks);
          let mut terminals = Vec::new();

          for mut referenced in referenced_tasks {
            Self::apply_command_options(&mut referenced.task, &command.task, &command_item.options);
            Self::inherit_failfast(command, &mut referenced);
            if let Some(terminal) = self
              .build_invocation(
                dag,
                &referenced,
                InvocationRequest {
                  context: context.with_overrides(complex.vars.clone(), complex.envs.clone()),
                  entry_parents: entries.clone(),
                  command_condition: command_item.options.condition.clone(),
                },
                None,
              )
              .await?
            {
              terminals.push(terminal);
            }
          }

          terminals
        },
        CommandPayload::Plugin(plugin) => {
          let simple = self.create_simple_command(plugin, command, &command_item.options);
          let task = self.create_task_node(dag, &simple, &context, command_item.options.condition.clone())?;
          Self::connect_parents(dag, &entries, &task)?;
          vec![task]
        },
      };

      let terminal = self.join_nodes(
        dag,
        &mut terminals,
        format!("Complete command in task {}", command.name),
      )?;
      if run_parallel {
        parallel_terminals.extend(terminal);
      } else if terminal.is_some() {
        sequential_parent = terminal;
      }
    }

    let mut terminals = if run_parallel {
      parallel_terminals
    } else {
      sequential_parent.into_iter().collect()
    };
    let terminal = self.join_nodes(dag, &mut terminals, format!("Complete task {}", command.name))?;
    let predecessors = terminal.map_or(parents, |terminal| vec![terminal]);

    self.attach_deferred_nodes(dag, deferred_nodes, predecessors)
  }

  fn connect_parents(dag: &mut DagNode, parents: &[ArcNode], task: &ArcNode) -> ExecutorResult<()> {
    for parent in parents {
      dag.add_dependency(parent, task)?;
    }
    Ok(())
  }

  fn join_nodes(&self, dag: &mut DagNode, nodes: &mut Vec<ArcNode>, name: String) -> ExecutorResult<Option<ArcNode>> {
    match nodes.len() {
      0 => Ok(None),
      1 => Ok(nodes.pop()),
      _ => {
        let group = self.create_group_node(dag, Some(AllowedRun::Always), name)?;
        for node in nodes.drain(..) {
          dag.add_dependency(&node, &group)?;
        }
        Ok(Some(group))
      },
    }
  }

  /// Compiles a deferred command into a nested execution plan.
  async fn create_deferred_node(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    deferred: &TaskCommand,
    context: InvocationContext,
    registered_after: Vec<String>,
  ) -> ExecutorResult<ArcNode> {
    let order = self.defer_order.fetch_add(1, Ordering::SeqCst);
    let name = format!("Deferred command {order} for {}", command.name);

    // Normalize `defer` into an ordinary one-command task so shell commands, task references,
    // and plugin commands all use the existing graph-building path.
    let deferred_command = FindResult {
      name: name.clone(),
      octafile: command.octafile.clone(),
      task: Task {
        cmds: Some(vec![TaskCommand {
          payload: deferred.payload.clone(),
          options: CommandOptions {
            platforms: None,
            deferred: false,
            ..deferred.options.clone()
          },
        }]),
        deps: None,
        platforms: None,
        condition: None,
        preconditions: None,
        sources: None,
        timeout: deferred.options.timeout.or(command.task.timeout),
        run: Some(AllowedRun::Always),
        plugin: None,
        ..command.task.clone()
      },
    };

    // Each deferred action owns its nested cleanup scope, including defers declared by a
    // referenced task. This keeps nested cleanup ordering local to that task invocation.
    let nested_builder = self.nested_builder();
    let mut deferred_dag = DAG::new();
    nested_builder
      .build_invocation(
        &mut deferred_dag,
        &deferred_command,
        InvocationRequest {
          context,
          entry_parents: Vec::new(),
          command_condition: None,
        },
        Some(false),
      )
      .await?;

    if deferred_dag.node_count() == 0 {
      nested_builder.create_group_node(&mut deferred_dag, Some(AllowedRun::Always), format!("Skipped {name}"))?;
    }

    let plan = ExecutionPlan::new(deferred_dag, nested_builder.deferred.into_inner().unwrap());

    // The barrier node preserves ordering in the main DAG. Its executable payload is stored
    // in `DeferredAction`, not in `TaskNode`.
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(name.clone())
      .node_kind(NodeKind::Barrier)
      .build()
      .unwrap();
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    self.deferred.lock().unwrap().insert(
      task.id.clone(),
      Arc::new(DeferredAction {
        command: name,
        plan,
        order,
        registered_after,
      }),
    );

    Ok(task)
  }

  fn nested_builder(&self) -> Self {
    // Runtime context is inherited, while cleanup order and collected actions belong to
    // the nested plan and therefore start from an empty state.
    Self {
      plugin_manager: self.plugin_manager.clone(),
      finder: self.finder.clone(),
      dir: self.dir.clone(),
      command_args: self.command_args.clone(),
      variable_overrides: self.variable_overrides.clone(),
      variable_resolver: self.variable_resolver.clone(),
      os_arch: self.os_arch.clone(),
      os_type: self.os_type.clone(),
      defer_order: AtomicUsize::new(0),
      deferred: Mutex::new(HashMap::new()),
    }
  }

  /// Connects cleanup barriers after the completed work in their declaration scope.
  fn attach_deferred_nodes(
    &self,
    dag: &mut DagNode,
    mut deferred_nodes: Vec<ArcNode>,
    mut predecessors: Vec<ArcNode>,
  ) -> ExecutorResult<Option<ArcNode>> {
    if deferred_nodes.is_empty() {
      return self.join_nodes(dag, &mut predecessors, "Complete task scope".to_string());
    }

    if predecessors.is_empty() {
      predecessors.push(self.create_group_node(
        dag,
        Some(AllowedRun::Always),
        "Register deferred commands".to_string(),
      )?);
    }

    // Reversing declaration order produces defer-N -> ... -> defer-1 (LIFO).
    deferred_nodes.reverse();
    for deferred in deferred_nodes {
      for predecessor in &predecessors {
        dag.add_dependency(predecessor, &deferred)?;
      }
      predecessors = vec![deferred];
    }

    Ok(predecessors.pop())
  }

  /// Creates a task node with the given configuration
  fn create_task_node(
    &self,
    dag: &mut DagNode,
    cmd: &FindResult,
    context: &InvocationContext,
    command_condition: Option<PluginCommand>,
  ) -> ExecutorResult<ArcNode> {
    let vars = self.collect_vars(cmd, context.vars.clone())?;
    let envs = self.collect_envs(cmd, context.envs.clone(), &vars)?;

    let plugin = cmd.task.plugin.clone().map(plugin_invocation).transpose()?;

    let mut conditions = context.conditions.per_command.clone();
    if let Some(condition) = command_condition {
      conditions.push(plugin_invocation(condition)?);
    }

    let task_config = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(cmd.name.clone())
      .dep_name(context.dep_name.clone())
      .dir(cmd.task.dir.clone().unwrap_or(cmd.octafile.dir.clone()))
      .vars(vars)
      .envs(envs)
      .condition_runtime(ConditionRuntime::command(
        conditions,
        context.conditions.guards.clone(),
        context.conditions.runtime_context.clone(),
      ))
      .preconditions(cmd.task.preconditions.clone())
      .timeout(cmd.task.timeout)
      .sources(cmd.task.sources.clone())
      .octafile_root(cmd.octafile.root().dir.clone())
      .silent(cmd.task.silent)
      .failfast(cmd.task.failfast.or(cmd.octafile.failfast))
      .source_strategy(cmd.task.source_strategy.clone())
      .ignore_errors(cmd.task.ignore_error)
      .run_mode(self.task_run_mode(cmd))
      .plugin(plugin);

    let task = TaskNode::new(task_config.build().unwrap());
    let arc_task = Arc::new(task);

    // Добавить созданную ноду в граф
    dag.add_node(arc_task.clone());

    Ok(arc_task)
  }

  /// Creates a single-evaluation condition node and adds its result to the task scope.
  fn add_condition_gate(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    mut request: GateRequest,
  ) -> ExecutorResult<(InvocationContext, ArcNode)> {
    let vars = self.collect_vars(command, request.context.vars.clone())?;
    let envs = self.collect_envs(command, request.context.envs.clone(), &vars)?;
    let state = Arc::new(ConditionState::default());
    let name = format!("{} condition for {}", request.phase.label(), command.name);
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(name)
      .dir(command.task.dir.clone().unwrap_or(command.octafile.dir.clone()))
      .vars(vars)
      .envs(envs)
      .condition_runtime(ConditionRuntime::gate(
        request.condition,
        state.clone(),
        request.context.conditions.guards.clone(),
      ))
      .timeout(command.task.timeout)
      .silent(Some(true))
      .failfast(command.task.failfast.or(command.octafile.failfast))
      .run_mode(Some(AllowedRun::Always))
      .node_kind(NodeKind::Condition)
      .build()
      .unwrap();
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    for parent in request.parents {
      dag.add_dependency(&parent, &task)?;
    }

    if matches!(request.phase, ConditionPhase::AfterDependencies) {
      request.context.conditions.runtime_context = Some(state.clone());
    }
    request.context.conditions.guards.push(state);
    Ok((request.context, task))
  }

  /// Builds the condition and dependency prefix shared by every task invocation.
  async fn prepare_invocation(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    mut request: InvocationRequest,
  ) -> ExecutorResult<PreparedInvocation> {
    // Runtime values may be shared inside one task, but never across a nested task invocation.
    request.context.conditions.runtime_context = None;
    let mut context = request.context;
    let mut parent = None;

    if let Some(condition) = request.command_condition {
      let (updated, gate) = self.add_condition_gate(
        dag,
        command,
        GateRequest {
          condition: plugin_invocation(condition)?,
          phase: ConditionPhase::Command,
          context,
          parents: request.entry_parents.clone(),
        },
      )?;
      context = updated;
      parent = Some(gate);
    }

    if let Some(condition) = command
      .task
      .condition
      .as_ref()
      .and_then(|conditions| conditions.before_deps.as_ref())
    {
      let parents = gate_or_parents(parent.as_ref(), &request.entry_parents);
      let (updated, gate) = self.add_condition_gate(
        dag,
        command,
        GateRequest {
          condition: plugin_invocation(condition.command.clone())?,
          phase: ConditionPhase::BeforeDependencies,
          context,
          parents,
        },
      )?;
      context = updated;
      parent = Some(gate);
    }

    let dependency_entries = gate_or_parents(parent.as_ref(), &request.entry_parents);
    let deps = self
      .process_dependencies(dag, command, dependency_entries.clone(), context.conditions.clone())
      .await?;
    parent = deps.or(parent);

    if let Some(condition) = command
      .task
      .condition
      .as_ref()
      .and_then(|conditions| conditions.after_deps.as_ref())
    {
      match condition.evaluate {
        ConditionEvaluation::Once => {
          let parents = gate_or_parents(parent.as_ref(), &request.entry_parents);
          let (updated, gate) = self.add_condition_gate(
            dag,
            command,
            GateRequest {
              condition: plugin_invocation(condition.command.clone())?,
              phase: ConditionPhase::AfterDependencies,
              context,
              parents,
            },
          )?;
          context = updated;
          parent = Some(gate);
        },
        ConditionEvaluation::PerCommand => context
          .conditions
          .per_command
          .push(plugin_invocation(condition.command.clone())?),
      }
    }

    let parents = gate_or_parents(parent.as_ref(), &request.entry_parents);
    Ok(PreparedInvocation { context, parents })
  }

  /// Process task dependencies and build the dependency graph
  ///
  /// # Arguments
  /// * `dag` - The DAG being built
  /// * `cmd` - The command containing dependencies
  /// * `parents` - Parent nodes in the graph
  ///
  /// # Returns
  /// Optional group node that contains all dependencies
  async fn process_dependencies(
    &self,
    dag: &mut DagNode,
    cmd: &FindResult,
    parents: Vec<ArcNode>,
    scope: ConditionScope,
  ) -> ExecutorResult<Option<ArcNode>> {
    let Some(deps) = &cmd.task.deps else {
      return Ok(None);
    };

    let deps_map = self.build_deps_frequency_map(deps);
    let mut terminals = Vec::new();

    for dep in deps {
      let (dep_name, vars, envs, timeout) = match dep {
        Deps::Simple(name) => (name.as_str(), None, None, None),
        Deps::Complex(dep) => (dep.task.as_str(), dep.vars.clone(), dep.envs.clone(), dep.timeout),
      };
      let mut dependencies = self.find_and_filter_commands(&cmd.octafile, dep_name)?;
      dependencies = self.filter_command_by_platform(dependencies);

      for mut dependency in dependencies {
        dependency.task.timeout = timeout.or(dependency.task.timeout);
        Self::inherit_failfast(cmd, &mut dependency);
        let task_name = self.generate_unique_task_name(dep_name, &deps_map);
        if let Some(terminal) = self
          .build_invocation(
            dag,
            &dependency,
            InvocationRequest {
              context: InvocationContext::new(task_name, vars.clone(), envs.clone(), scope.clone()),
              entry_parents: parents.clone(),
              command_condition: None,
            },
            None,
          )
          .await?
        {
          terminals.push(terminal);
        }
      }
    }

    if terminals.is_empty() {
      return Ok(None);
    }

    let group = self.create_group_node(
      dag,
      self.task_run_mode(cmd),
      format!("Group deps task for command {}", cmd.name),
    )?;
    Self::connect_parents(dag, &terminals, &group)?;
    Ok(Some(group))
  }

  /// Generate a unique task name based on frequency map
  fn generate_unique_task_name(&self, task_name: &str, deps_map: &HashMap<&str, (usize, usize)>) -> String {
    if let Some(&(count, index)) = deps_map.get(task_name) {
      if count > 1 {
        format!("{}_{}", task_name, index + 1)
      } else {
        task_name.to_string()
      }
    } else {
      task_name.to_string()
    }
  }

  fn filter_internal_task(&self, tasks: Vec<FindResult>) -> Vec<FindResult> {
    tasks
      .into_iter()
      .filter(|t| !t.task.internal.unwrap_or(false))
      .collect()
  }

  fn task_run_mode(&self, cmd: &FindResult) -> Option<AllowedRun> {
    cmd.task.run.clone().or_else(|| cmd.octafile.run.clone())
  }

  fn task_failfast(cmd: &FindResult) -> bool {
    cmd.task.failfast.or(cmd.octafile.failfast).unwrap_or(false)
  }

  /// A task invocation belongs to both the caller and the referenced task fail-fast scopes.
  fn inherit_failfast(parent: &FindResult, child: &mut FindResult) {
    child.task.failfast = Some(Self::task_failfast(parent) || Self::task_failfast(child));
  }

  /// Build a map tracking frequency of each dependency
  fn build_deps_frequency_map<'a>(&self, deps: &'a [Deps]) -> HashMap<&'a str, (usize, usize)> {
    let mut deps_map = HashMap::new();

    for dep in deps {
      match dep {
        Deps::Simple(name) => {
          deps_map
            .entry(name.as_str())
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, 0));
        },
        Deps::Complex(complex) => {
          deps_map
            .entry(complex.task.as_str())
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, 0));
        },
      }
    }

    deps_map
  }

  /// Find and filter commands by platform
  fn find_and_filter_commands(&self, octafile: &Arc<Octafile>, task_name: &str) -> ExecutorResult<Vec<FindResult>> {
    let cmds = self.finder.find_by_path(Arc::clone(octafile), task_name);

    if cmds.is_empty() {
      return Err(ExecutorError::CommandNotFound(task_name.to_string()));
    }

    Ok(cmds)
  }

  /// Create a simple command from a complex one
  fn create_simple_command(
    &self,
    plugin: &PluginCommand,
    command: &FindResult,
    options: &CommandOptions,
  ) -> FindResult {
    let mut task = Task {
      cmds: None,
      plugin: Some(plugin.clone()),
      deps: None,
      ..command.task.clone()
    };
    Self::apply_command_options(&mut task, &command.task, options);

    FindResult {
      name: match &plugin.value {
        serde_yml::Value::String(command) => command.clone(),
        value => value.to_string(),
      },
      octafile: command.octafile.clone(),
      task,
    }
  }

  /// Applies command metadata while retaining defaults from its containing and referenced tasks.
  fn apply_command_options(task: &mut Task, containing_task: &Task, options: &CommandOptions) {
    task.timeout = options.timeout.or(containing_task.timeout).or(task.timeout);
    task.silent = options.silent.or(containing_task.silent).or(task.silent);
    task.ignore_error = options
      .ignore_error
      .or(containing_task.ignore_error)
      .or(task.ignore_error);
  }

  fn create_group_node(
    &self,
    dag: &mut DagNode,
    run: Option<AllowedRun>,
    name: String,
  ) -> ExecutorResult<Arc<TaskNode>> {
    let task_config = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .run_mode(run)
      .dep_name(name)
      .node_kind(NodeKind::Barrier);

    let task = TaskNode::new(task_config.build().unwrap());
    let arc_task = Arc::new(task);

    dag.add_node(arc_task.clone());

    Ok(arc_task)
  }

  /// Collects variables from global, hierarchy and task levels
  fn collect_vars(&self, cmd: &FindResult, execute_vars: Option<octa_octafile::Vars>) -> ExecutorResult<Vars> {
    let mut vars = self.initialize_global_vars(cmd);
    let env_vars: HashMap<String, String> = env::vars().collect();

    self.process_hierarchy_vars(cmd, &mut vars);
    vars = self.add_task_vars(cmd, vars);
    if let Some(exec_vars) = execute_vars {
      vars.extend_variables(exec_vars);
    }

    vars.extend_with(&env_vars);

    if !self.variable_overrides.is_empty() {
      // Keep overrides in their own layer so their templates can consume every configured value,
      // while declaration order remains meaningful between CLI options.
      let mut overrides = Vars::with_parent(vars);
      for (name, value) in &self.variable_overrides {
        overrides.insert(name, value);
      }
      vars = overrides;
    }

    vars.resolve_required(self.variable_resolver.as_deref())?;
    Ok(vars)
  }

  fn initialize_global_vars(&self, cmd: &FindResult) -> Vars {
    let os_type = whoami::platform();
    let os_arch = whoami::cpu_arch();
    let root = cmd.octafile.root();

    let mut vars = root.vars.clone().map(Vars::with_variables).unwrap_or_default();
    vars.set_dir(root.dir.clone());

    vars.insert("ROOT_DIR", &root.dir.display().to_string());
    vars.insert("OCTAFILE_DIR", &root.dir.display().to_string());
    vars.insert("USER_WORKING_DIR", &self.dir.display().to_string());
    vars.insert("COMMAND_ARGS", &self.command_args);
    vars.insert("OCTA_OS", &os_type.to_string());
    vars.insert("OCTA_ARCH", &os_arch.to_string());

    vars
  }

  fn process_hierarchy_vars(&self, cmd: &FindResult, vars: &mut Vars) {
    let full_path = cmd.octafile.hierarchy_path();
    let mut current = Arc::clone(cmd.octafile.root());

    debug!(
      "Processing hierarchy variables for command {} in path {}",
      cmd.name,
      full_path.join(":")
    );

    for segment in full_path {
      match current.get_included(&segment).unwrap() {
        Some(nested_octafile) => {
          let mut new_vars = Vars::new();
          new_vars.set_parent(Some(vars.clone()));
          if let Some(nested_vars) = nested_octafile.vars.clone() {
            new_vars.set_variables(nested_vars);
          }
          new_vars.set_dir(nested_octafile.dir.clone());
          new_vars.insert("TASKFILE_DIR", &current.dir.display().to_string());

          *vars = new_vars;
          current = Arc::clone(&nested_octafile);
          debug!("Updated variables for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }
  }

  fn add_task_vars(&self, cmd: &FindResult, vars: Vars) -> Vars {
    // Add variables from current task
    match cmd.task.vars.clone() {
      Some(task_vars) => {
        let mut new_vars = Vars::with_variables_and_parent(task_vars, vars);
        new_vars.set_dir(self.variable_working_dir(cmd));

        new_vars
      },
      None => {
        let mut new_vars = Vars::new();
        new_vars.set_parent(Some(vars));
        new_vars.set_dir(self.variable_working_dir(cmd));

        new_vars
      },
    }
  }

  /// Collects environments from global, hierarchy and task levels
  fn collect_envs(
    &self,
    cmd: &FindResult,
    execute_envs: Option<octa_octafile::Envs>,
    vars: &Vars,
  ) -> ExecutorResult<Envs> {
    // Dotenv path templates are resolved while the hierarchy is built, so keep a flattened
    // view of the process environment and all environment layers processed so far.
    let mut template_environment = env::vars().collect::<HashMap<_, _>>();
    let mut envs = self.initialize_global_envs(cmd, vars, &mut template_environment)?;
    self.process_hierarchy_envs(cmd, vars, &mut envs, &mut template_environment)?;
    envs = self.add_task_envs(cmd, vars, envs, &mut template_environment)?;
    if let Some(exec_vars) = execute_envs {
      envs.extend(exec_vars.clone());
    }
    Ok(envs)
  }

  fn initialize_global_envs(
    &self,
    cmd: &FindResult,
    vars: &Vars,
    template_environment: &mut HashMap<String, String>,
  ) -> ExecutorResult<Envs> {
    let mut envs = Envs::new();
    let root = cmd.octafile.root();
    envs.set_dir(root.dir.clone());
    let mut values = Self::dotenv_values(dotenv::load(
      root.dotenv.as_deref(),
      &root.dir,
      vars,
      template_environment,
    )?);

    // Explicit Octafile values take precedence over values loaded from dotenv files.
    if let Some(env) = &root.env {
      values.extend(env.clone());
    }
    Self::extend_template_environment(template_environment, &values);
    envs.set_value(values);

    Ok(envs)
  }

  fn process_hierarchy_envs(
    &self,
    cmd: &FindResult,
    vars: &Vars,
    envs: &mut Envs,
    template_environment: &mut HashMap<String, String>,
  ) -> ExecutorResult<()> {
    let full_path = cmd.octafile.hierarchy_path();
    let mut current = Arc::clone(cmd.octafile.root());

    debug!(
      "Processing hierarchy environments for command {} in path {}",
      cmd.name,
      full_path.join(":")
    );

    for segment in full_path {
      match current.get_included(&segment).unwrap() {
        Some(nested_octafile) => {
          let mut new_envs = Envs::new();
          new_envs.set_parent(Some(envs.clone()));
          new_envs.set_dir(nested_octafile.dir.clone());
          let mut values = Self::dotenv_values(dotenv::load(
            nested_octafile.dotenv.as_deref(),
            &nested_octafile.dir,
            vars,
            template_environment,
          )?);
          // Each included Octafile overrides its dotenv values explicitly and then overrides
          // all environment values inherited from its parent.
          if let Some(env) = &nested_octafile.env {
            values.extend(env.clone());
          }
          Self::extend_template_environment(template_environment, &values);
          new_envs.set_value(values);

          *envs = new_envs;
          current = Arc::clone(&nested_octafile);
          debug!("Updated environments for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }

    Ok(())
  }

  fn add_task_envs(
    &self,
    cmd: &FindResult,
    vars: &Vars,
    envs: Envs,
    template_environment: &mut HashMap<String, String>,
  ) -> ExecutorResult<Envs> {
    let mut new_envs = Envs::new();
    new_envs.set_parent(Some(envs));

    let task_dir = self.task_working_dir(cmd);
    let mut values = Self::dotenv_values(dotenv::load(
      cmd.task.dotenv.as_deref(),
      &task_dir,
      vars,
      template_environment,
    )?);
    // Task values are the most specific layer and therefore override task dotenv values.
    if let Some(task_envs) = &cmd.task.env {
      values.extend(task_envs.clone());
    }
    Self::extend_template_environment(template_environment, &values);
    new_envs.set_value(values);

    Ok(new_envs)
  }

  fn dotenv_values(values: HashMap<String, String>) -> octa_octafile::Envs {
    values
      .into_iter()
      .map(|(key, value)| (key, EnvValue::String(value)))
      .collect()
  }

  fn extend_template_environment(target: &mut HashMap<String, String>, values: &octa_octafile::Envs) {
    // Dynamic values are evaluated only when the task runs and therefore cannot select a
    // dotenv file while the execution graph is being built.
    target.extend(
      values
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned()))),
    );
  }

  fn task_working_dir(&self, cmd: &FindResult) -> PathBuf {
    let task_dir = cmd.task.dir.as_ref().unwrap_or(&cmd.octafile.dir);
    if task_dir.is_absolute() {
      task_dir.clone()
    } else {
      self.dir.join(task_dir)
    }
  }

  fn variable_working_dir(&self, cmd: &FindResult) -> PathBuf {
    let task_dir = self.task_working_dir(cmd);
    let value = task_dir.to_string_lossy();
    // A templated task directory cannot be resolved until variables have been expanded.
    if value.contains("{{") && value.contains("}}") {
      cmd.octafile.dir.clone()
    } else {
      task_dir
    }
  }

  fn filter_command_by_platform(&self, commands: Vec<FindResult>) -> Vec<FindResult> {
    commands
      .into_iter()
      .filter(|cmd| self.matches_platforms(cmd.task.platforms.as_deref()))
      .collect()
  }

  fn matches_platforms(&self, platforms: Option<&[String]>) -> bool {
    platforms.is_none_or(|platforms| {
      platforms
        .iter()
        .any(|platform| matches_platform(platform, &self.os_type, &self.os_arch))
    })
  }

  fn validate_dag(&self, dag: &DagNode, command: &str) -> ExecutorResult<()> {
    if dag.node_count() == 0 {
      return Err(ExecutorError::TaskNotFound(command.to_string()));
    }

    if dag.has_cycle()? {
      return Err(ExecutorError::CycleDetected);
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use octa_dag::Identifiable;
  use octa_octafile::Octafile;
  use std::fs;
  use tempfile::TempDir;

  use crate::vars::VariablePrompt;

  struct TestVariableResolver;

  impl VariableResolver for TestVariableResolver {
    fn resolve(&self, _prompt: &VariablePrompt) -> Result<String, String> {
      Ok("value".to_owned())
    }
  }

  fn create_test_task() -> Task {
    Task {
      plugin: Some(PluginCommand {
        key: "shell".to_string(),
        value: serde_yml::Value::String("echo test".to_string()),
      }),
      ..Task::default()
    }
  }

  #[test]
  fn test_matches_platform_and_architecture() {
    for selector in ["linux", "x86_64", "amd64", "x64", "linux/x86_64", "linux/amd64"] {
      assert!(matches_platform(selector, "linux", "x86_64"), "{selector} must match");
    }

    for selector in ["windows", "arm64", "linux/arm64", "windows/amd64"] {
      assert!(
        !matches_platform(selector, "linux", "x86_64"),
        "{selector} must not match"
      );
    }

    assert!(matches_platform(" macOS / AARCH64 ", "macos", "arm64"));
    assert!(matches_platform("darwin/amd64", "macos", "x86_64"));
  }

  #[tokio::test]
  async fn test_task_graph_builder_new() -> ExecutorResult<()> {
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?.with_variable_resolver(Arc::new(TestVariableResolver));
    assert!(builder.command_args.is_empty());
    assert!(builder.variable_overrides.is_empty());
    assert!(builder.variable_resolver.is_some());
    assert!(builder.dir.exists());
    Ok(())
  }

  #[tokio::test]
  async fn test_build_simple_task() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test:
          shell: echo "test"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let dag = builder.build(octafile, "test", true, vec![]).await?;

    assert_eq!(dag.node_count(), 1);
    assert!(!dag.has_cycle()?);
    let tasks: Vec<String> = dag.nodes().iter().map(|n| n.name.clone()).collect();

    assert!(tasks.contains(&"test".to_owned()));
    Ok(())
  }

  #[tokio::test]
  async fn test_command_timeout_inherits_and_overrides_task_timeout() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        called:
          timeout: 20s
          shell: echo called
        pipeline:
          timeout: 10s
          cmds:
            - echo inherited
            - task: called
              timeout: 2s
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let dag = TaskGraphBuilder::new(plugin_manager)?
      .build(octafile, "pipeline", false, vec![])
      .await?;

    let inherited = dag.nodes().iter().find(|task| task.name == "echo inherited").unwrap();
    let overridden = dag.nodes().iter().find(|task| task.name == "called").unwrap();
    assert_eq!(
      inherited.timeout.unwrap().duration(),
      std::time::Duration::from_secs(10)
    );
    assert_eq!(
      overridden.timeout.unwrap().duration(),
      std::time::Duration::from_secs(2)
    );

    Ok(())
  }

  #[tokio::test]
  async fn test_command_options_inherit_and_override_task_defaults() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        called:
          if: referenced-condition
          silent: true
          ignore_error: true
          shell: echo called
        pipeline:
          if: containing-condition
          silent: true
          ignore_error: true
          cmds:
            - shell: echo inherited
            - shell: echo overridden
              if: command-condition
              silent: false
              ignore_error: false
            - task: called
              if: reference-condition
              silent: false
              ignore_error: false
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let dag = TaskGraphBuilder::new(plugin_manager)?
      .build(octafile, "pipeline", false, vec![])
      .await?;

    let inherited = dag.nodes().iter().find(|task| task.name == "echo inherited").unwrap();
    assert!(inherited.conditions().is_empty());
    assert!(inherited.silent);
    assert!(inherited.ignore_errors);

    let overridden = dag.nodes().iter().find(|task| task.name == "echo overridden").unwrap();
    assert_eq!(overridden.conditions(), ["command-condition"]);
    assert!(!overridden.silent);
    assert!(!overridden.ignore_errors);

    let referenced = dag.nodes().iter().find(|task| task.name == "called").unwrap();
    assert!(referenced.conditions().is_empty());
    assert!(!referenced.silent);
    assert!(!referenced.ignore_errors);

    let gates = dag
      .nodes()
      .iter()
      .filter(|task| task.name.contains("condition for"))
      .map(|task| {
        assert!(crate::task::TaskItem::requires_concurrency_permit(task.as_ref()));
        task.conditions()
      })
      .collect::<Vec<_>>();
    assert!(gates.contains(&vec!["containing-condition".to_string()]));
    assert!(gates.contains(&vec!["referenced-condition".to_string()]));
    assert!(gates.contains(&vec!["reference-condition".to_string()]));

    Ok(())
  }

  #[tokio::test]
  async fn test_failfast_inherits_and_overrides_octafile_default() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      failfast: true
      tasks:
        inherited:
          shell: echo inherited
        overridden:
          failfast: false
          shell: echo overridden
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let dag = TaskGraphBuilder::new(plugin_manager)?
      .build(octafile, "**", true, vec![])
      .await?;

    let inherited = dag.nodes().iter().find(|task| task.name == "inherited").unwrap();
    let overridden = dag.nodes().iter().find(|task| task.name == "overridden").unwrap();
    assert!(inherited.failfast);
    assert!(!overridden.failfast);

    Ok(())
  }

  #[tokio::test]
  async fn test_failfast_is_inherited_by_task_references_and_dependencies() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        pipeline:
          failfast: true
          execute_mode: parallel
          deps:
            - dependency
          cmds:
            - task: called
        dependency:
          failfast: false
          shell: echo dependency
        called:
          failfast: false
          shell: echo called
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let dag = TaskGraphBuilder::new(plugin_manager)?
      .build(octafile, "pipeline", true, vec![])
      .await?;

    let dependency = dag.nodes().iter().find(|task| task.name == "dependency").unwrap();
    let called = dag.nodes().iter().find(|task| task.name == "called").unwrap();
    assert!(dependency.failfast);
    assert!(called.failfast);

    Ok(())
  }

  #[tokio::test]
  async fn test_octafile_run_mode_is_inherited_and_can_be_overridden() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      run: once
      includes:
        child: child.yml
      tasks:
        inherited:
          shell: echo inherited
        overridden:
          run: changed
          shell: echo overridden
    "#;
    let child_content = r#"
      version: 1
      run: changed
      tasks:
        child_inherited:
          shell: echo child
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;
    fs::write(temp_dir.path().join("child.yml"), child_content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let dag = builder.build(octafile, "**", true, vec![]).await?;

    let inherited = dag.nodes().iter().find(|task| task.name == "inherited").unwrap();
    let overridden = dag.nodes().iter().find(|task| task.name == "overridden").unwrap();
    let child = dag
      .nodes()
      .iter()
      .find(|task| task.name == "child:child_inherited")
      .unwrap();
    assert_eq!(inherited.run_mode, task::RunMode::Once);
    assert_eq!(overridden.run_mode, task::RunMode::Changed);
    assert_eq!(child.run_mode, task::RunMode::Changed);

    Ok(())
  }

  #[tokio::test]
  async fn test_build_with_dependencies() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        task1:
          shell: echo "task1"
        task2:
          shell: echo "task2"
          deps:
            - task1
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let dag = builder.build(octafile, "task2", true, vec![]).await?;

    let id_to_name: HashMap<String, String> = dag
      .nodes()
      .iter()
      .map(|item| (item.name.clone(), item.id.clone()))
      .collect();

    assert_eq!(dag.node_count(), 3);
    assert!(!dag.has_cycle()?);
    let tasks: Vec<String> = dag.nodes().iter().map(|n| n.name.clone()).collect();
    assert!(tasks.contains(&"task1".to_owned()));
    assert!(tasks.contains(&"task2".to_owned()));

    assert!(dag.edges().contains_key(&id_to_name["task1"]));

    Ok(())
  }

  #[tokio::test]
  async fn test_command_not_found() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test:
          shell: echo "test"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let result = builder.build(octafile, "nonexistent", true, vec![]).await;

    assert!(matches!(result, Err(ExecutorError::CommandNotFound(_))));
    Ok(())
  }

  #[tokio::test]
  async fn test_platform_specific_tasks() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test_macos:
          shell: echo "test"
          platforms:
            - macos
        test_linux:
          shell: echo "test"
          platforms:
            - linux
        test_windows:
          shell: echo "test"
          platforms:
            - windows
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;

    let dag = if cfg!(target_os = "linux") {
      builder.build(octafile, "test_linux", true, vec![]).await?
    } else if cfg!(target_os = "windows") {
      builder.build(octafile, "test_windows", true, vec![]).await?
    } else {
      builder.build(octafile, "test_macos", true, vec![]).await?
    };

    // The number of nodes will depend on the current platform
    assert!(!dag.has_cycle()?);
    Ok(())
  }

  #[tokio::test]
  async fn test_platform_mismatch_creates_skipped_plan() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test:
          platforms: [unsupported]
          shell: echo test
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let dag = builder.build(octafile, "test", true, vec![]).await?;

    assert_eq!(dag.node_count(), 1);
    assert!(dag.nodes().iter().all(|task| task.is_internal()));
    Ok(())
  }

  #[tokio::test]
  async fn test_command_with_args() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test:
          shell: echo "{{ COMMAND_ARGS }}"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let args = vec!["arg1".to_string(), "arg2".to_string()];
    let dag = builder.build(octafile, "test", true, args).await?;

    assert_eq!(dag.node_count(), 1);
    Ok(())
  }

  #[tokio::test]
  async fn test_variable_inheritance() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      vars:
        GLOBAL: "global"
      tasks:
        test:
          vars:
            LOCAL: "local"
          shell: echo "{{ GLOBAL }} {{ LOCAL }}"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?.with_variable_overrides(vec![
      ("GLOBAL".to_owned(), "override".to_owned()),
      ("COMBINED".to_owned(), "{{ LOCAL }}-override".to_owned()),
    ]);
    let dag = builder.build(octafile, "test", true, vec![]).await?;

    assert_eq!(dag.node_count(), 1);
    let task = dag.nodes().iter().find(|task| task.name == "test").unwrap();
    let mut vars = task.vars.clone();
    vars.expand(true).await?;
    assert_eq!(vars.get("GLOBAL").and_then(|value| value.as_str()), Some("override"));
    assert_eq!(vars.get("LOCAL").and_then(|value| value.as_str()), Some("local"));
    assert_eq!(
      vars.get("COMBINED").and_then(|value| value.as_str()),
      Some("local-override")
    );
    Ok(())
  }

  #[tokio::test]
  async fn test_dotenv_layers_and_search() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let nested_dir = temp_dir.path().join("work").join("nested");
    fs::create_dir_all(temp_dir.path().join("config"))?;
    fs::create_dir_all(&nested_dir)?;
    fs::write(
      temp_dir.path().join("Octafile.yml"),
      r#"
        version: 1
        vars:
          PROFILE: local
        dotenv:
          - .env.{{ PROFILE }}
          - config/base.env
        env:
          EXPLICIT_ROOT: root
        tasks:
          test:
            dir: work/nested
            dotenv:
              - .env.task
            env:
              TASK_VALUE: explicit
            shell: echo test
      "#,
    )?;
    fs::write(
      temp_dir.path().join(".env.local"),
      "ROOT_PRIORITY=first\nROOT_DOTENV=loaded\n",
    )?;
    fs::write(
      temp_dir.path().join("config").join("base.env"),
      "ROOT_PRIORITY=second\n",
    )?;
    fs::write(
      temp_dir.path().join("work").join(".env.task"),
      "TASK_DOTENV=searched\nTASK_VALUE=dotenv\n",
    )?;

    let octafile = Octafile::load(
      Some(temp_dir.path().join("Octafile.yml")),
      false,
      vec!["shell".to_string()],
      "shell",
    )?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let mut builder = TaskGraphBuilder::new(plugin_manager)?;
    builder.dir = temp_dir.path().to_path_buf();
    let plan = builder.build(octafile, "test", true, vec![]).await?;
    let task = plan.nodes().iter().find(|task| task.name == "test").unwrap();
    let mut envs = task.envs.clone();
    envs.expand().await?;

    assert_eq!(envs.get("ROOT_PRIORITY"), Some(&"first".to_string()));
    assert_eq!(envs.get("ROOT_DOTENV"), Some(&"loaded".to_string()));
    assert_eq!(envs.get("EXPLICIT_ROOT"), Some(&"root".to_string()));
    assert_eq!(envs.get("TASK_DOTENV"), Some(&"searched".to_string()));
    assert_eq!(envs.get("TASK_VALUE"), Some(&"explicit".to_string()));
    Ok(())
  }

  #[tokio::test]
  async fn test_environment_inheritance() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      env:
        GLOBAL_ENV: "global"
      tasks:
        test:
          env:
            LOCAL_ENV: "local"
          shell: echo "test"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let dag = builder.build(octafile, "test", true, vec![]).await?;

    assert_eq!(dag.node_count(), 1);
    let task = dag.nodes().iter().find(|task| task.name == "test").unwrap();
    let mut envs = task.envs.clone();
    envs.expand().await?;
    assert_eq!(envs.get("GLOBAL_ENV"), Some(&"global".to_string()));
    assert_eq!(envs.get("LOCAL_ENV"), Some(&"local".to_string()));
    Ok(())
  }

  #[tokio::test]
  async fn test_process_hierarchy_vars() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let root_octafile = setup_test_octafiles(&temp_dir).await?;

    let nested_octafile = root_octafile.get_included("nested")?.unwrap();
    let deep_octafile = nested_octafile.get_included("deep")?.unwrap();

    let cmd = FindResult {
      name: "test_cmd".to_string(),
      octafile: deep_octafile,
      task: create_test_task(),
    };

    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let mut vars = Vars::new();
    builder.process_hierarchy_vars(&cmd, &mut vars);

    vars.expand(false).await?;

    // Updated assertions for Tera values
    assert_eq!(vars.get("NESTED_VAR").and_then(|v| v.as_str()), Some("nested_value"));
    assert_eq!(vars.get("DEEP_VAR").and_then(|v| v.as_str()), Some("deep_value"));
    assert!(vars.get("TASKFILE_DIR").is_some());

    Ok(())
  }

  #[tokio::test]
  async fn test_nested_includes() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let root_octafile = setup_test_octafiles(&temp_dir).await?;

    let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
    let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
    let builder = TaskGraphBuilder::new(plugin_manager)?;
    let dag = builder.build(root_octafile, "**:deep_task", true, vec![]).await?;

    assert!(dag.node_count() > 0);
    assert!(!dag.has_cycle()?);
    Ok(())
  }

  async fn setup_test_octafiles(temp_dir: &TempDir) -> ExecutorResult<Arc<Octafile>> {
    // Create root octafile content
    let root_content = r#"
      version: 1
      vars:
        ROOT_VAR: "root_value"
      includes:
        nested:
          octafile: nested/Octafile.yml
      tasks:
        root_task:
          shell: echo "root"
    "#;

    // Create nested octafile content
    let nested_content = r#"
      version: 1
      vars:
        NESTED_VAR: "nested_value"
      includes:
        deep:
          octafile: deep/Octafile.yml
      tasks:
        nested_task:
          shell: echo "nested"
    "#;

    // Create deep octafile content
    let deep_content = r#"
      version: 1
      vars:
        DEEP_VAR: "deep_value"
      tasks:
        deep_task:
          shell: echo "deep"
    "#;

    // Create directory structure and write files
    let root_path = temp_dir.path().join("Octafile.yml");
    let nested_dir = temp_dir.path().join("nested");
    let deep_dir = nested_dir.join("deep");
    std::fs::create_dir(&nested_dir)?;
    std::fs::create_dir(&deep_dir)?;
    let nested_path = nested_dir.join("Octafile.yml");
    let deep_path = deep_dir.join("Octafile.yml");

    std::fs::write(&root_path, root_content)?;
    std::fs::write(&nested_path, nested_content)?;
    std::fs::write(&deep_path, deep_content)?;

    // Load the root octafile
    Ok(Octafile::load(
      Some(root_path),
      false,
      vec!["shell".to_string()],
      "shell",
    )?)
  }
}
