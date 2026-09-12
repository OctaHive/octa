//! Expansion of task invocations into executable DAG nodes.
//!
//! An Octafile task is hierarchical: it may have conditions, dependencies,
//! nested task calls, plugin commands, task-result cache boundaries, and deferred cleanup.
//! The scheduler intentionally knows none of those concepts, so this module
//! lowers them into a flat graph of executable and barrier nodes.
//!
//! Every builder method that appends a subgraph returns its terminal node. The
//! caller connects later work to that terminal, which makes sequential and
//! parallel composition use the same representation. Barrier nodes join or
//! order subgraphs without consuming executor concurrency.

use super::*;

impl TaskGraphBuilder {
  /// Expands one task invocation and returns the node that represents its completion.
  ///
  /// The boxed call is the indirection required by recursive async expansion:
  /// task bodies can invoke other tasks, which enter this method again.
  pub(super) async fn build_invocation(
    &mut self,
    dag: &mut DagNode,
    command: &FindResult,
    request: InvocationRequest,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<Option<ArcNode>> {
    self.validate_selected_task_outputs(command)?;
    Box::pin(self._build_invocation(dag, command, request, run_parallel)).await
  }

  async fn _build_invocation(
    &mut self,
    dag: &mut DagNode,
    command: &FindResult,
    mut request: InvocationRequest,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<Option<ArcNode>> {
    // A scope identifies an invocation, not merely a task definition. Calling
    // the same task twice therefore produces distinct lifecycle and output IDs.
    let silence = self
      .force_silence
      .or(command.task.silent)
      .or(command.octafile.silent)
      .unwrap_or_default();
    let static_prefix = command
      .task
      .prefix
      .as_ref()
      .filter(|prefix| !prefix.contains("{{"))
      .cloned();
    let parent_task_id = request
      .context
      .output_scope
      .as_ref()
      .map(ConsoleScope::id)
      .or(request.context.parent_task_id);
    let scope = self.scope_allocator.scope_with_parent_options(
      command.name.clone(),
      parent_task_id,
      static_prefix,
      silence.hides_stdout(),
      silence.hides_stderr(),
    );
    scope.set_render_mode(
      command
        .task
        .presentation
        .as_ref()
        .and_then(|presentation| presentation.output)
        .map(task_output_mode),
    );
    self.scopes.push(scope.clone());
    request.context.output_scope = Some(scope);

    // Watching is a property of the declared input set, not of persistent
    // caching. Register it once per distinct invocation input contract so a
    // non-cached task can still participate in `--watch`.
    if let Some(inputs) = command.task.files.as_ref().and_then(|files| files.inputs.as_ref()) {
      let target = WatchTarget::new(inputs.clone(), self.dir.clone());
      if !self
        .watch_targets
        .iter()
        .any(|existing| existing.inputs == target.inputs && existing.workspace == target.workspace)
      {
        self.watch_targets.push(target);
      }
    }

    // Variables and environments are collected once per invocation and shared
    // by all of its condition, cache, command, and barrier nodes.
    let collected_vars = self.collect_vars_with_identity(command, request.context.vars.clone())?;
    self.validate_task_output_references(command, &collected_vars.runtime)?;
    let environment = self.collect_environment_plan(command, request.context.envs.clone())?;
    request.context.runtime = Some(Arc::new(task::InvocationRuntime::new(
      collected_vars.runtime,
      environment,
      collected_vars.identity_names,
      self.variable_resolver.clone(),
    )));
    let interactive = command.task.interactive.unwrap_or(false);
    let mut prepared = self.prepare_invocation(dag, command, request).await?;

    // All nodes in an interactive body share one session ID so the executor
    // holds a single exclusive runtime guard across the complete invocation.
    if interactive && prepared.context.interactive_session.is_none() {
      prepared.context.interactive_session = Some(Uuid::new_v4().to_string());
    }
    self
      .build_task_body(dag, command, prepared.context, prepared.parents, run_parallel)
      .await
  }

  /// Validates structured references while the complete task namespace is available.
  fn validate_task_output_references(&self, command: &FindResult, vars: &Vars) -> ExecutorResult<()> {
    let references = vars.task_output_references();
    if references.is_empty() {
      return Ok(());
    }
    if command
      .task
      .condition
      .as_ref()
      .is_some_and(|condition| condition.before_deps.is_some())
    {
      return Err(ExecutorError::InvalidTaskOutputReference {
        variable: references[0].name.clone(),
        message: "task output variables cannot be used with a before_deps condition".to_owned(),
      });
    }
    let dependency_names = command
      .task
      .deps
      .as_deref()
      .unwrap_or_default()
      .iter()
      .map(|dependency| match dependency {
        Deps::Simple(name) => name.as_str(),
        Deps::Complex(dependency) => dependency.task.as_str(),
      })
      .collect::<Vec<_>>();

    for TaskOutputVariable {
      name: variable,
      reference,
      ..
    } in references
    {
      if dependency_names.iter().filter(|name| **name == reference.task).count() != 1 {
        return Err(ExecutorError::InvalidTaskOutputReference {
          variable,
          message: format!("'{}' must name exactly one direct dependency", reference.task),
        });
      }
      let resolved =
        self.filter_command_by_platform(self.find_and_filter_commands(&command.octafile, &reference.task)?);
      if resolved.len() != 1 {
        return Err(ExecutorError::InvalidTaskOutputReference {
          variable,
          message: format!(
            "dependency '{}' must resolve to exactly one task on this platform",
            reference.task
          ),
        });
      }
      if !resolved[0]
        .task
        .outputs
        .as_ref()
        .is_some_and(|outputs| outputs.contains_key(&reference.output))
      {
        return Err(ExecutorError::InvalidTaskOutputReference {
          variable,
          message: format!("dependency '{}' does not export '{}'", reference.task, reference.output),
        });
      }
    }
    Ok(())
  }

  /// Ensures platform filtering leaves one producer for every declared task output.
  fn validate_selected_task_outputs(&self, command: &FindResult) -> ExecutorResult<()> {
    let Some(outputs) = &command.task.outputs else {
      return Ok(());
    };
    let commands = command.task.cmds.as_deref().unwrap_or_default();
    for (name, output) in outputs {
      let producers = commands
        .iter()
        .filter(|candidate| candidate.options.id.as_deref() == Some(output.step.as_str()))
        .filter(|candidate| self.matches_platforms(candidate.options.platforms.as_deref()))
        .count();
      if producers != 1 {
        return Err(ExecutorError::InvalidTaskOutput {
          task: command.name.clone(),
          output: name.clone(),
          message: format!(
            "step '{}' has {producers} executable producers on this platform; expected exactly one",
            output.step
          ),
        });
      }
    }
    Ok(())
  }

  async fn build_task_body(
    &mut self,
    dag: &mut DagNode,
    command: &FindResult,
    context: InvocationContext,
    parents: Vec<ArcNode>,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<Option<ArcNode>> {
    // Persistent lookup belongs after dependencies and task-level conditions.
    // Each dependency owns its own cache boundary and is evaluated separately.
    let (context, parents, cache_plan) = self.add_cache_lookup(dag, command, context, parents).await?;

    // Shorthand tasks contain one plugin payload directly instead of `cmds`.
    let Some(commands) = &command.task.cmds else {
      let task = self.create_task_node(
        command,
        &context,
        None,
        None,
        Self::command_cache_key(&command.name, 0),
        0,
      )?;
      dag.add_node(task.clone());
      Self::connect_parents(dag, &parents, &task)?;
      let terminal = self.add_resource_registration(dag, command, &context, vec![task])?;
      return self.add_cache_finalize(dag, command, &context, cache_plan, terminal.into_iter().collect());
    };

    // Nested task calls publish their own scopes. Explicit outer barriers keep
    // the containing invocation open around those child lifecycles; a body of
    // only plugin commands already starts and finishes its scope through those
    // executable nodes.
    let lifecycle_scope = if commands
      .iter()
      .any(|command| matches!(command.payload, CommandPayload::Task(_)))
    {
      context.output_scope.clone()
    } else {
      None
    };
    let parents = if let Some(scope) = &lifecycle_scope {
      let start = self.create_scope_barrier(dag, format!("Start task scope {}", command.name), scope.clone())?;
      Self::connect_parents(dag, &parents, &start)?;
      vec![start]
    } else {
      parents
    };

    let run_parallel = run_parallel.unwrap_or(matches!(command.task.execute_mode, Some(ExecuteMode::Parallel)));
    let mut sequential_parent = None;
    let mut parallel_terminals = Vec::new();
    let mut deferred_nodes = Vec::new();

    for (command_index, command_item) in commands.iter().enumerate() {
      // Platform-excluded commands contribute no node and therefore cannot
      // accidentally block the remaining body.
      if !self.matches_platforms(command_item.options.platforms.as_deref()) {
        continue;
      }

      // Parallel commands share the invocation entry. Sequential commands use
      // the preceding command's terminal after the first iteration.
      let entries = if run_parallel {
        parents.clone()
      } else {
        sequential_parent
          .clone()
          .map_or_else(|| parents.clone(), |parent| vec![parent])
      };

      if command_item.options.deferred {
        // Registration is defined by reachability: the executor runs this
        // cleanup only if every entry node below has actually completed.
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
            &simple,
            &context,
            command_item.options.condition.clone(),
            command_item.options.id.as_deref(),
            Self::command_cache_key(&command.name, command_index),
            command_index,
          )?;
          dag.add_node(task.clone());
          Self::connect_parents(dag, &entries, &task)?;
          vec![task]
        },
      };

      let terminal = self.join_nodes(
        dag,
        &mut terminals,
        format!("Complete command in task {}", command.name),
      )?;
      // A nested task selector may expand to several matching tasks. Joining
      // here makes that entire expansion behave like one command in sequence.
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
    // Parallel bodies need one completion point before cache finalization and
    // deferred ordering. Sequential bodies already have a single tail.
    let terminal = self.join_nodes(dag, &mut terminals, format!("Complete task {}", command.name))?;
    let predecessors = terminal.map_or(parents, |terminal| vec![terminal]);
    // Cleanup may mutate declared outputs. Run it before validating resources
    // and capturing the bundle, otherwise a hit could restore pre-cleanup state.
    let predecessors = self
      .attach_deferred_nodes(dag, deferred_nodes, predecessors)?
      .into_iter()
      .collect();
    let predecessors = self
      .add_resource_registration(dag, command, &context, predecessors)?
      .into_iter()
      .collect();
    let terminal = self.add_cache_finalize(dag, command, &context, cache_plan, predecessors)?;

    // The closing scope barrier is required only when the explicit opening
    // barrier above was inserted for a body containing nested task calls.
    let Some(scope) = lifecycle_scope else {
      return Ok(terminal);
    };
    let Some(terminal) = terminal else {
      return Ok(None);
    };
    let finish = self.create_scope_barrier(dag, format!("Finish task scope {}", command.name), scope)?;
    dag.add_dependency(&terminal, &finish)?;
    Ok(Some(finish))
  }

  /// Builds the ordered condition/dependency prefix for a task invocation.
  ///
  /// The order is fixed: command-call condition, `before_deps`, dependencies,
  /// then an `after_deps` condition. An `after_deps` condition configured as
  /// per-command is attached to body nodes instead of becoming a gate.
  async fn prepare_invocation(
    &mut self,
    dag: &mut DagNode,
    command: &FindResult,
    request: InvocationRequest,
  ) -> ExecutorResult<PreparedInvocation> {
    let mut context = request.context;
    let mut parent = None;

    // A condition placed on a task-reference command belongs outside the
    // referenced task's own conditions and dependencies.
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

    // Dependencies fan out from the latest gate and are joined back into one
    // terminal before the task body begins.
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

  /// Expands all declared dependencies and joins their terminal nodes.
  ///
  /// Dependencies are siblings: each starts from the same incoming parents.
  /// Overrides on a complex dependency are applied only to that invocation.
  /// `None` means no dependency produced a runnable node.
  async fn process_dependencies(
    &mut self,
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
      let (dep_name, vars, envs, timeout, quiet, silent, raw, interactive) = match dep {
        Deps::Simple(name) => (name.as_str(), None, None, None, None, None, None, None),
        Deps::Complex(dep) => (
          dep.task.as_str(),
          dep.vars.clone(),
          dep.envs.clone(),
          dep.timeout,
          dep.quiet,
          dep.silent,
          dep.raw,
          dep.interactive,
        ),
      };
      // One selector can resolve multiple tasks (for example through project
      // namespaces), so every match becomes a distinct invocation branch.
      let mut dependencies = self.find_and_filter_commands(&cmd.octafile, dep_name)?;
      dependencies = self.filter_command_by_platform(dependencies);

      for mut dependency in dependencies {
        dependency.task.timeout = timeout.or(dependency.task.timeout);
        dependency.task.quiet = quiet.or(dependency.task.quiet);
        dependency.task.silent = silent.or(dependency.task.silent);
        dependency.task.raw = raw.or(dependency.task.raw);
        dependency.task.interactive = interactive.or(dependency.task.interactive);
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

  /// Adds a stable ordinal when the same dependency is declared more than once.
  ///
  /// The first repeated occurrence is suffixed with `_1`; a dependency that
  /// appears only once retains its configured name for readable output.
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

  /// Removes tasks marked internal from wildcard/user-facing lookup results.
  pub(super) fn filter_internal_task(&self, tasks: Vec<FindResult>) -> Vec<FindResult> {
    tasks
      .into_iter()
      .filter(|t| !t.task.internal.unwrap_or(false))
      .collect()
  }

  /// Resolves task-level run mode with the Octafile default as fallback.
  pub(super) fn task_run_mode(&self, cmd: &FindResult) -> Option<AllowedRun> {
    cmd.task.run.clone().or_else(|| cmd.octafile.run.clone())
  }

  /// Resolves fail-fast as an effective boolean for inheritance and scheduling.
  fn task_failfast(cmd: &FindResult) -> bool {
    cmd.task.failfast.or(cmd.octafile.failfast).unwrap_or(false)
  }

  /// Resolves a task-reference selector and applies call-site execution options.
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

  /// Counts dependency names and initializes their emitted-name ordinal.
  ///
  /// Each value is `(total_occurrences, emitted_occurrences)`. The second value
  /// is advanced by [`Self::generate_unique_task_name`].
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

  /// Resolves a task selector and turns an empty result into a planning error.
  ///
  /// Platform filtering remains a separate step because dependency and nested
  /// command call sites apply different overrides before building invocations.
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

  /// Normalizes one plugin command into the same task shape as shorthand tasks.
  ///
  /// Reusing `create_task_node` after this conversion keeps timeout, silence,
  /// raw mode, error handling, and template behavior identical across syntaxes.
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
    task.quiet = options.quiet.or(containing_task.quiet).or(task.quiet);
    task.silent = options.silent.or(containing_task.silent).or(task.silent);
    task.raw = options.raw.or(containing_task.raw).or(task.raw);
    task.ignore_error = options
      .ignore_error
      .or(containing_task.ignore_error)
      .or(task.ignore_error);
  }

  /// Rejects empty plans and dependency cycles before the scheduler sees them.
  pub(super) fn validate_dag(&self, dag: &DagNode, command: &str) -> ExecutorResult<()> {
    if dag.node_count() == 0 {
      return Err(ExecutorError::TaskNotFound(command.to_string()));
    }

    if dag.has_cycle()? {
      return Err(ExecutorError::CycleDetected);
    }

    self.validate_parallel_cache_outputs(dag)?;

    Ok(())
  }
}

/// Converts parser-level task output configuration into renderer-level mode.
pub(super) fn task_output_mode(mode: TaskOutputMode) -> RenderMode {
  match mode {
    TaskOutputMode::Interleaved => RenderMode::Interleaved,
    TaskOutputMode::Group => RenderMode::Group,
    TaskOutputMode::Prefixed => RenderMode::Prefixed,
    TaskOutputMode::OnError => RenderMode::OnError,
    TaskOutputMode::KeepOrder => RenderMode::KeepOrder,
    TaskOutputMode::Replacing => RenderMode::Replacing,
    TaskOutputMode::Timed => RenderMode::Timed,
  }
}
