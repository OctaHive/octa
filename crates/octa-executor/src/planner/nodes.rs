//! Construction of executable, condition, resource, and barrier DAG nodes.
//!
//! `graph` decides which subgraphs an invocation needs and in what order.
//! This module performs the mechanical lowering into `TaskNode` values and
//! owns deferred subplans. Keeping that distinction local to the planner makes
//! the recursive control flow readable without adding a second graph-builder
//! abstraction or exposing node construction outside this module.

use super::*;

impl TaskGraphBuilder {
  /// Appends one task-level collector after the successful body.
  pub(super) fn add_resource_registration(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    context: &InvocationContext,
    mut predecessors: Vec<ArcNode>,
  ) -> ExecutorResult<Option<ArcNode>> {
    let artifacts = command.task.artifacts.clone().unwrap_or_default();
    let reports = command.task.reports.clone().unwrap_or_default();
    if artifacts.is_empty() && reports.is_empty() {
      return self.join_nodes(dag, &mut predecessors, format!("Complete task {}", command.name));
    }

    let name = format!("Register resources for {}", command.name);
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(command.name.clone())
      .dir(self.task_working_dir(command))
      .workspace(self.dir.clone())
      .condition_runtime(ConditionRuntime::command(Vec::new(), context.conditions.guards.clone()))
      .task_cache_runtime(TaskCacheRuntime::resources(
        context.cache_guards.clone(),
        context.task_cache.clone(),
      ))
      .execution_binding(context.output_scope.clone().map(ExecutionBinding::for_task))
      .interactive_session(context.interactive_session.clone())
      .silent(Some(true))
      .failfast(command.task.failfast.or(command.octafile.failfast))
      .action(NodeAction::RegisterResources { artifacts, reports })
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    Self::connect_parents(dag, &predecessors, &task)?;
    Ok(Some(task))
  }

  /// Connects every incoming terminal to the first node of a new subgraph.
  pub(super) fn connect_parents(dag: &mut DagNode, parents: &[ArcNode], task: &ArcNode) -> ExecutorResult<()> {
    for parent in parents {
      dag.add_dependency(parent, task)?;
    }
    Ok(())
  }

  /// Returns one terminal for zero, one, or many parallel branches.
  ///
  /// The single-node case is returned directly; only true fan-in allocates a
  /// barrier. Besides reducing graph size, this preserves the original node as
  /// the lifecycle endpoint whenever no join is needed.
  pub(super) fn join_nodes(
    &self,
    dag: &mut DagNode,
    nodes: &mut Vec<ArcNode>,
    name: String,
  ) -> ExecutorResult<Option<ArcNode>> {
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
  ///
  /// The main graph receives only a registration/ordering barrier. The nested
  /// plan is retained in `DeferredAction` and can therefore run during normal
  /// traversal or shutdown without teaching the scheduler about command shapes.
  pub(super) async fn create_deferred_node(
    &mut self,
    dag: &mut DagNode,
    command: &FindResult,
    deferred: &TaskCommand,
    context: InvocationContext,
    registered_after: Vec<String>,
  ) -> ExecutorResult<ArcNode> {
    let order = self.defer_order;
    self.defer_order += 1;
    let name = format!("Deferred command {order} for {}", command.name);
    let cache_guards = context.cache_guards.clone();

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
        files: None,
        cache: None,
        timeout: deferred.options.timeout.or(command.task.timeout),
        run: Some(AllowedRun::Always),
        plugin: None,
        ..command.task.clone()
      },
    };

    // Each deferred action owns its nested cleanup scope, including defers declared by a
    // referenced task. This keeps nested cleanup ordering local to that task invocation.
    let mut nested_builder = self.nested_builder();
    let mut deferred_dag = DAG::new();
    nested_builder
      .build_invocation(
        &mut deferred_dag,
        &deferred_command,
        InvocationRequest {
          context: context.deferred(),
          entry_parents: Vec::new(),
          command_condition: None,
        },
        Some(false),
      )
      .await?;

    if deferred_dag.node_count() == 0 {
      nested_builder.create_group_node(&mut deferred_dag, Some(AllowedRun::Always), format!("Skipped {name}"))?;
    }

    let plan = ExecutionPlan::new(deferred_dag, nested_builder.deferred, nested_builder.scopes);

    // The barrier node preserves ordering in the main DAG. Its executable payload is stored
    // in `DeferredAction`, not in `TaskNode`.
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(name.clone())
      .task_cache_runtime(TaskCacheRuntime::guarded(cache_guards))
      .action(NodeAction::Barrier)
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    self.deferred.insert(
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

  /// Creates an isolated collector for a nested deferred plan.
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
      secret_session: self.secret_session.clone(),
      scope_allocator: self.scope_allocator.clone(),
      force_quiet: self.force_quiet,
      force_silence: self.force_silence,
      force_raw: self.force_raw,
      scopes: Vec::new(),
      os_arch: self.os_arch.clone(),
      os_type: self.os_type.clone(),
      defer_order: 0,
      deferred: HashMap::new(),
      // Deferred cleanup is not part of the watched build body. Its input
      // contracts are evaluated only when the cleanup plan actually runs.
      watch_targets: Vec::new(),
    }
  }

  /// Connects cleanup barriers after completed work in reverse declaration order.
  ///
  /// Empty predecessors receive a synthetic registration root so a task made
  /// only of defers still has a reachable graph. Whether an unvisited cleanup is
  /// eligible during shutdown is decided from `registered_after` by Executor.
  pub(super) fn attach_deferred_nodes(
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

  /// Creates one executable plugin node from normalized task configuration.
  ///
  /// A plugin node receives a step binding; internal barrier-like nodes receive
  /// only the task binding. This is where the `run -> task -> step` identity
  /// hierarchy becomes attached to executable graph nodes.
  pub(super) fn create_task_node(
    &self,
    cmd: &FindResult,
    context: &InvocationContext,
    command_condition: Option<PluginCommand>,
    configured_step_id: Option<&str>,
    cache_key: String,
    cache_position: usize,
  ) -> ExecutorResult<ArcNode> {
    let plugin = cmd.task.plugin.clone().map(plugin_invocation).transpose()?;

    // Per-command conditions inherited from the invocation are evaluated on
    // every executable node; an inline command condition is appended last.
    let mut conditions = context.conditions.per_command.clone();
    if let Some(condition) = command_condition {
      conditions.push(plugin_invocation(condition)?);
    }
    let runtime = context
      .runtime
      .clone()
      .ok_or(ExecutorError::TaskConfigFieldMissing("invocation_runtime"))?;

    let id = Uuid::new_v4().to_string();
    let execution_binding = context.output_scope.clone().map(|scope| {
      if let Some(plugin) = &plugin {
        let step = self
          .scope_allocator
          .step(&scope, configured_step_id.unwrap_or_else(|| plugin.key()));
        ExecutionBinding::for_step(scope, step)
      } else {
        ExecutionBinding::for_task(scope)
      }
    });
    let task_config = TaskConfig::builder()
      .id(id)
      .name(cmd.name.clone())
      .dep_name(context.dep_name.clone())
      .cache_key(cache_key)
      .dir(self.task_working_dir(cmd))
      .workspace(self.dir.clone())
      .vars(runtime.vars().clone())
      .envs(runtime.configured_envs())
      .invocation_runtime(Some(runtime))
      .condition_runtime(ConditionRuntime::command(conditions, context.conditions.guards.clone()))
      .task_cache_runtime(TaskCacheRuntime::command(
        context.cache_guards.clone(),
        context.task_cache.clone(),
        cache_position,
      ))
      // A cached invocation evaluates preconditions once on its lookup node.
      // Uncached commands retain the historical per-command behavior.
      .preconditions(
        context
          .task_cache
          .is_none()
          .then(|| cmd.task.preconditions.clone())
          .flatten(),
      )
      .timeout(cmd.task.timeout)
      .execution_binding(execution_binding)
      .prefix_template(cmd.task.prefix.clone())
      .step_exports(step_exports(&cmd.task, configured_step_id))
      .interactive_session(context.interactive_session.clone())
      .silent(self.force_silence.or(cmd.task.silent).or(cmd.octafile.silent))
      .quiet(if self.force_quiet {
        Some(true)
      } else {
        cmd.task.quiet.or(cmd.octafile.quiet)
      })
      .raw(if self.force_raw || context.interactive_session.is_some() {
        Some(true)
      } else {
        cmd.task.raw.or(cmd.octafile.raw)
      })
      .failfast(cmd.task.failfast.or(cmd.octafile.failfast))
      .ignore_errors(cmd.task.ignore_error)
      .run_mode(self.task_run_mode(cmd))
      .plugin(plugin);

    Ok(Arc::new(TaskNode::new(task_config.build()?)))
  }

  /// Returns a cache identity shared by repeated invocations of the same
  /// task definition, but not by separate commands with equal payloads.
  pub(super) fn command_cache_key(task_name: &str, command_index: usize) -> String {
    format!("{task_name}::command[{command_index}]")
  }

  /// Creates a single-evaluation condition node and adds its result to the task scope.
  ///
  /// Descendant nodes receive the same `ConditionState` as a guard. They can
  /// skip without reevaluating the plugin and fail explicitly if graph ordering
  /// ever lets them observe an unpublished decision.
  pub(super) fn add_condition_gate(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    mut request: GateRequest,
  ) -> ExecutorResult<(InvocationContext, ArcNode)> {
    let runtime = request
      .context
      .runtime
      .clone()
      .ok_or(ExecutorError::TaskConfigFieldMissing("invocation_runtime"))?;
    let state = Arc::new(ConditionState::default());
    let name = format!("{} condition for {}", request.phase.label(), command.name);
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(name)
      .dir(self.task_working_dir(command))
      .vars(runtime.vars().clone())
      .envs(runtime.configured_envs())
      .invocation_runtime(Some(runtime))
      .condition_runtime(ConditionRuntime::gate(
        request.condition,
        state.clone(),
        request.context.conditions.guards.clone(),
      ))
      .task_cache_runtime(TaskCacheRuntime::default())
      .execution_binding(request.context.output_scope.clone().map(ExecutionBinding::for_task))
      .interactive_session(request.context.interactive_session.clone())
      .timeout(command.task.timeout)
      .silent(Some(true))
      .failfast(command.task.failfast.or(command.octafile.failfast))
      .run_mode(Some(AllowedRun::Always))
      .action(NodeAction::Condition)
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    for parent in request.parents {
      dag.add_dependency(&parent, &task)?;
    }

    request.context.conditions.guards.push(state);
    Ok((request.context, task))
  }

  /// Creates a zero-work node used only for graph fan-in or ordering.
  pub(super) fn create_group_node(
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
      .action(NodeAction::Barrier);

    let task = Arc::new(TaskNode::new(task_config.build()?));
    dag.add_node(task.clone());
    Ok(task)
  }

  /// Creates a zero-work node that participates in a task scope lifecycle.
  ///
  /// These barriers bracket outer task scopes that contain nested task calls;
  /// ordinary join barriers deliberately remain unscoped.
  pub(super) fn create_scope_barrier(
    &self,
    dag: &mut DagNode,
    name: String,
    output_scope: ConsoleScope,
  ) -> ExecutorResult<ArcNode> {
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(name)
      .execution_binding(Some(ExecutionBinding::for_task(output_scope)))
      .action(NodeAction::Barrier)
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    Ok(task)
  }
}

fn step_exports(task: &Task, step_id: Option<&str>) -> HashMap<String, StepExport> {
  let Some(step_id) = step_id else {
    return HashMap::new();
  };
  task
    .outputs
    .as_ref()
    .into_iter()
    .flat_map(|outputs| outputs.iter())
    .filter(|(_, output)| output.step == step_id)
    .map(|(name, output)| {
      (
        name.clone(),
        StepExport {
          field: output.field.clone(),
          secret: output.secret,
        },
      )
    })
    .collect()
}
