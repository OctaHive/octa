//! Expansion of task invocations into executable DAG nodes.

use super::*;

impl TaskGraphBuilder {
  pub(super) async fn build_invocation(
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
    mut request: InvocationRequest,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<Option<ArcNode>> {
    let scope = self
      .scope_allocator
      .scope_with_prefix(command.name.clone(), command.task.prefix.clone());
    self
      .scopes
      .lock()
      .map_err(|error| ExecutorError::LockError(error.to_string()))?
      .push(scope.clone());
    request.context.output_scope = Some(scope);
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
    let collected_vars = self.collect_vars_with_identity(command, context.vars.clone())?;
    let envs = self.collect_envs(command, context.envs.clone(), &collected_vars.runtime)?;
    let (context, parents, freshness) =
      self.add_freshness_gate(dag, command, context, parents, &collected_vars, &envs)?;
    let vars = collected_vars.runtime;

    let Some(commands) = &command.task.cmds else {
      let task = self.create_task_node(dag, command, &context, &vars, &envs, None)?;
      Self::connect_parents(dag, &parents, &task)?;
      return self.add_freshness_commit(dag, command, &context, freshness, vec![task]);
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
          let mut terminals = Vec::new();

          for referenced in self.resolve_referenced_tasks(command, command_item, &complex.task)? {
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
          let task = self.create_task_node(
            dag,
            &simple,
            &context,
            &vars,
            &envs,
            command_item.options.condition.clone(),
          )?;
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
    let predecessors = self
      .add_freshness_commit(dag, command, &context, freshness, predecessors)?
      .into_iter()
      .collect();

    self.attach_deferred_nodes(dag, deferred_nodes, predecessors)
  }

  fn add_freshness_gate(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    mut context: InvocationContext,
    parents: Vec<ArcNode>,
    vars: &CollectedVars,
    envs: &Envs,
  ) -> ExecutorResult<(InvocationContext, Vec<ArcNode>, Option<Arc<FreshnessState>>)> {
    if command.task.sources.is_none() && command.task.output.is_none() {
      return Ok((context, parents, None));
    }

    let state = Arc::new(FreshnessState::default());
    let method = Self::task_source_strategy(command)
      .map(SourceMethod::from)
      .unwrap_or(SourceMethod::Hash);
    let strategy = self.source_strategies.resolve(&method)?;
    let identity = FreshnessIdentity::new(
      command.name.clone(),
      context.dep_name.clone(),
      self.execution_definition(command)?,
    )
    .with_invocation_inputs(
      self.command_args.clone(),
      self.variable_overrides.clone(),
      context.vars.clone(),
      context.envs.clone(),
    );
    let spec = FreshnessConfig::new(
      command.task.sources.clone(),
      command.task.output.clone(),
      command.octafile.root().dir.clone(),
      method,
      strategy,
    )
    .spec(identity)
    .track_variables(vars.identity_names.clone());
    let name = format!("Check freshness for {}", command.name);
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(command.name.clone())
      .dir(command.task.dir.clone().unwrap_or(command.octafile.dir.clone()))
      .vars(vars.runtime.clone())
      .envs(envs.clone())
      .condition_runtime(ConditionRuntime::command(
        Vec::new(),
        context.conditions.guards.clone(),
        context.conditions.runtime_context.clone(),
      ))
      .freshness_runtime(FreshnessRuntime::guarded(context.freshness.clone()))
      .output_scope(context.output_scope.clone())
      .silent(Some(true))
      .failfast(command.task.failfast.or(command.octafile.failfast))
      .action(NodeAction::FreshnessCheck {
        spec: Box::new(spec),
        state: state.clone(),
      })
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    Self::connect_parents(dag, &parents, &task)?;

    context.freshness = Some(state.clone());
    Ok((context, vec![task], Some(state)))
  }

  /// Commits the source fingerprint after every non-deferred command in this invocation succeeds.
  fn add_freshness_commit(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    context: &InvocationContext,
    state: Option<Arc<FreshnessState>>,
    predecessors: Vec<ArcNode>,
  ) -> ExecutorResult<Option<ArcNode>> {
    let Some(state) = state else {
      let mut predecessors = predecessors;
      return self.join_nodes(dag, &mut predecessors, format!("Complete task {}", command.name));
    };

    let name = format!("Commit freshness for {}", command.name);
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(command.name.clone())
      .freshness_runtime(FreshnessRuntime::guarded(context.freshness.clone()))
      .output_scope(context.output_scope.clone())
      .silent(Some(true))
      .failfast(command.task.failfast.or(command.octafile.failfast))
      .action(NodeAction::FreshnessCommit(state))
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    Self::connect_parents(dag, &predecessors, &task)?;
    Ok(Some(task))
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
        output: None,
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

    let deferred = nested_builder
      .deferred
      .into_inner()
      .map_err(|error| ExecutorError::LockError(error.to_string()))?;
    let scopes = nested_builder
      .scopes
      .into_inner()
      .map_err(|error| ExecutorError::LockError(error.to_string()))?;
    let plan = ExecutionPlan::new(deferred_dag, deferred, scopes);

    // The barrier node preserves ordering in the main DAG. Its executable payload is stored
    // in `DeferredAction`, not in `TaskNode`.
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .dep_name(name.clone())
      .action(NodeAction::Barrier)
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    self
      .deferred
      .lock()
      .map_err(|error| ExecutorError::LockError(error.to_string()))?
      .insert(
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
      source_strategies: self.source_strategies.clone(),
      scope_allocator: self.scope_allocator.clone(),
      scopes: Mutex::new(Vec::new()),
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
    vars: &Vars,
    envs: &Envs,
    command_condition: Option<PluginCommand>,
  ) -> ExecutorResult<ArcNode> {
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
      .vars(vars.clone())
      .envs(envs.clone())
      .condition_runtime(ConditionRuntime::command(
        conditions,
        context.conditions.guards.clone(),
        context.conditions.runtime_context.clone(),
      ))
      .freshness_runtime(FreshnessRuntime::guarded(context.freshness.clone()))
      .preconditions(cmd.task.preconditions.clone())
      .timeout(cmd.task.timeout)
      .output_scope(context.output_scope.clone())
      .silent(cmd.task.silent)
      .failfast(cmd.task.failfast.or(cmd.octafile.failfast))
      .ignore_errors(cmd.task.ignore_error)
      .run_mode(self.task_run_mode(cmd))
      .plugin(plugin);

    let task = TaskNode::new(task_config.build()?);
    let arc_task = Arc::new(task);

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
      .freshness_runtime(FreshnessRuntime::guarded(request.context.freshness.clone()))
      .output_scope(request.context.output_scope.clone())
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
      .process_dependencies(dag, command, dependency_entries.clone(), context.clone())
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
    scope: InvocationContext,
  ) -> ExecutorResult<Option<ArcNode>> {
    let Some(deps) = &cmd.task.deps else {
      return Ok(None);
    };

    let mut deps_map = Self::build_deps_frequency_map(deps);
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
        let task_name = Self::generate_unique_task_name(dep_name, &mut deps_map);
        let dependency_context = scope.nested(task_name, vars.clone(), envs.clone());
        if let Some(terminal) = self
          .build_invocation(
            dag,
            &dependency,
            InvocationRequest {
              context: dependency_context,
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
  pub(super) fn generate_unique_task_name(task_name: &str, deps_map: &mut HashMap<&str, (usize, usize)>) -> String {
    if let Some((count, index)) = deps_map.get_mut(task_name) {
      if *count > 1 {
        *index += 1;
        format!("{task_name}_{index}")
      } else {
        task_name.to_string()
      }
    } else {
      task_name.to_string()
    }
  }

  pub(super) fn filter_internal_task(&self, tasks: Vec<FindResult>) -> Vec<FindResult> {
    tasks
      .into_iter()
      .filter(|t| !t.task.internal.unwrap_or(false))
      .collect()
  }

  fn task_run_mode(&self, cmd: &FindResult) -> Option<AllowedRun> {
    cmd.task.run.clone().or_else(|| cmd.octafile.run.clone())
  }

  fn task_source_strategy(cmd: &FindResult) -> Option<SourceStrategies> {
    cmd
      .task
      .source_strategy
      .clone()
      .or_else(|| cmd.octafile.source_strategy.clone())
  }

  fn task_failfast(cmd: &FindResult) -> bool {
    cmd.task.failfast.or(cmd.octafile.failfast).unwrap_or(false)
  }

  /// Captures the task definition together with inherited execution defaults.
  ///
  /// Referenced tasks own separate freshness boundaries and therefore keep
  /// their definitions and resolved inputs out of the caller's identity.
  fn execution_definition(&self, command: &FindResult) -> ExecutorResult<serde_json::Value> {
    Ok(serde_json::json!({
      "name": command.name,
      "task": task_identity::task_definition(&command.task)?,
      "effective": {
        "dir": self.task_working_dir(command),
        "run": self.task_run_mode(command),
        "source_strategy": Self::task_source_strategy(command),
        "failfast": Self::task_failfast(command),
      },
    }))
  }

  fn resolve_referenced_tasks(
    &self,
    parent: &FindResult,
    command: &TaskCommand,
    task_name: &str,
  ) -> ExecutorResult<Vec<FindResult>> {
    let tasks = self.find_and_filter_commands(&parent.octafile, task_name)?;
    Ok(
      self
        .filter_command_by_platform(tasks)
        .into_iter()
        .map(|mut task| {
          Self::apply_command_options(&mut task.task, &parent.task, &command.options);
          Self::inherit_failfast(parent, &mut task);
          task
        })
        .collect(),
    )
  }

  /// A task invocation belongs to both the caller and the referenced task fail-fast scopes.
  fn inherit_failfast(parent: &FindResult, child: &mut FindResult) {
    child.task.failfast = Some(Self::task_failfast(parent) || Self::task_failfast(child));
  }

  /// Build a map tracking frequency of each dependency
  pub(super) fn build_deps_frequency_map(deps: &[Deps]) -> HashMap<&str, (usize, usize)> {
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
  pub(super) fn find_and_filter_commands(
    &self,
    octafile: &Arc<Octafile>,
    task_name: &str,
  ) -> ExecutorResult<Vec<FindResult>> {
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

    let task = TaskNode::new(task_config.build()?);
    let arc_task = Arc::new(task);

    dag.add_node(arc_task.clone());

    Ok(arc_task)
  }

  pub(super) fn validate_dag(&self, dag: &DagNode, command: &str) -> ExecutorResult<()> {
    if dag.node_count() == 0 {
      return Err(ExecutorError::TaskNotFound(command.to_string()));
    }

    if dag.has_cycle()? {
      return Err(ExecutorError::CycleDetected);
    }

    Ok(())
  }
}
