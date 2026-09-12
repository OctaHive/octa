//! Cache-specific lowering and validation for task DAGs.
//!
//! Keeping these rules beside the planner, but outside general graph expansion,
//! makes the cache boundary explicit without introducing another trait or
//! crate. The module owns only planning decisions; lookup, restoration, and
//! publication remain in `result_cache`.

use super::*;

impl TaskGraphBuilder {
  pub(super) async fn add_cache_lookup(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    mut context: InvocationContext,
    parents: Vec<ArcNode>,
  ) -> ExecutorResult<(InvocationContext, Vec<ArcNode>, Option<Arc<TaskCachePlan>>)> {
    let Some(cache_config) = &command.task.cache else {
      return Ok((context, parents, None));
    };

    let files = command
      .task
      .files
      .as_ref()
      .ok_or_else(|| ExecutorError::InvalidCacheConfiguration("cached task has no files contract".to_owned()))?;
    let inputs = files
      .inputs
      .clone()
      .ok_or_else(|| ExecutorError::InvalidCacheConfiguration("cached task has no files.inputs contract".to_owned()))?;
    self.validate_cacheable_task(command, &inputs, &files.outputs)?;
    let outputs = files
      .outputs
      .iter()
      .map(|path| RelativePath::new(path.clone()))
      .collect::<Result<Vec<_>, _>>()
      .map_err(|error| ExecutorError::InvalidCacheConfiguration(error.to_string()))?;
    let state = Arc::new(TaskCacheState::default());
    let runtime = context
      .runtime
      .clone()
      .ok_or(ExecutorError::TaskConfigFieldMissing("invocation_runtime"))?;
    let definition = self.execution_definition(command)?;
    let plan = Arc::new(TaskCachePlan {
      workspace: self.dir.clone(),
      inputs,
      outputs,
      task_definition: TaskCachePlan::definition_digest(&definition)?,
      environment: cache_config.environment.clone(),
      // Values, conditions, and preconditions are tracked at runtime. Command
      // plugins have not executed at lookup time and are collected statically.
      plugin_keys: task_plugin_keys(&command.task),
      arguments: self.command_args.clone(),
      timeout: command.task.timeout.map(|timeout| timeout.duration()),
      salt: cache_config.salt.clone(),
    });
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(format!("Lookup cache for {}", command.name))
      .dep_name(command.name.clone())
      .dir(self.task_working_dir(command))
      .vars(runtime.vars().clone())
      .envs(runtime.configured_envs())
      .invocation_runtime(Some(runtime))
      .condition_runtime(ConditionRuntime::command(Vec::new(), context.conditions.guards.clone()))
      .task_cache_runtime(TaskCacheRuntime::guarded(context.cache_guards.clone()))
      .execution_binding(context.output_scope.clone().map(ExecutionBinding::for_task))
      .interactive_session(context.interactive_session.clone())
      .silent(Some(true))
      .failfast(command.task.failfast.or(command.octafile.failfast))
      .preconditions(command.task.preconditions.clone())
      .action(NodeAction::CacheLookup {
        plan: plan.clone(),
        state: state.clone(),
      })
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    Self::connect_parents(dag, &parents, &task)?;

    context.task_cache = Some(state.clone());
    context.cache_guards.push(state);
    Ok((context, vec![task], Some(plan)))
  }

  /// Appends publication after commands and resource registration succeed.
  pub(super) fn add_cache_finalize(
    &self,
    dag: &mut DagNode,
    command: &FindResult,
    context: &InvocationContext,
    plan: Option<Arc<TaskCachePlan>>,
    predecessors: Vec<ArcNode>,
  ) -> ExecutorResult<Option<ArcNode>> {
    let Some(plan) = plan else {
      let mut predecessors = predecessors;
      return self.join_nodes(dag, &mut predecessors, format!("Complete task {}", command.name));
    };

    let state = context
      .task_cache
      .clone()
      .ok_or(ExecutorError::TaskConfigFieldMissing("task_cache"))?;
    let (owner, ancestors) = context
      .cache_guards
      .split_last()
      .ok_or(ExecutorError::TaskConfigFieldMissing("task_cache_guard"))?;
    if !Arc::ptr_eq(owner, &state) {
      return Err(ExecutorError::TaskFailed(
        "cache guard ownership does not match the task cache state".to_owned(),
      ));
    }
    let task = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(format!("Finalize cache for {}", command.name))
      .dep_name(command.name.clone())
      // Finalization must run on this invocation's own hit, but an outer hit
      // suppresses the complete nested invocation including its finalizer.
      .task_cache_runtime(TaskCacheRuntime::guarded(ancestors.to_vec()))
      // A skipped task has neither a lookup decision nor a publishable result.
      .condition_runtime(ConditionRuntime::command(Vec::new(), context.conditions.guards.clone()))
      .execution_binding(context.output_scope.clone().map(ExecutionBinding::for_task))
      .interactive_session(context.interactive_session.clone())
      .silent(Some(true))
      .failfast(command.task.failfast.or(command.octafile.failfast))
      .action(NodeAction::CacheFinalize { plan, state })
      .build()?;
    let task = Arc::new(TaskNode::new(task));
    dag.add_node(task.clone());
    Self::connect_parents(dag, &predecessors, &task)?;
    Ok(Some(task))
  }

  /// Rejects task shapes that cannot be replayed completely and validates the
  /// filesystem contract with the same grammar used by snapshots and watch.
  fn validate_cacheable_task(
    &self,
    command: &FindResult,
    inputs: &[String],
    output_values: &[String],
  ) -> ExecutorResult<()> {
    if command.task.raw == Some(true)
      || command.task.interactive == Some(true)
      || command.task.ignore_error == Some(true)
    {
      return Err(ExecutorError::InvalidCacheConfiguration(format!(
        "task '{}' is raw, interactive, or ignores failures",
        command.name
      )));
    }
    if command
      .task
      .cmds
      .as_deref()
      .unwrap_or_default()
      .iter()
      .any(|command| matches!(command.payload, CommandPayload::Task(_)))
    {
      return Err(ExecutorError::InvalidCacheConfiguration(format!(
        "task '{}' contains a nested task call; place the cache boundary on executable leaf tasks",
        command.name
      )));
    }
    if command
      .task
      .outputs
      .as_ref()
      .is_some_and(|outputs| outputs.values().any(|output| output.secret))
    {
      return Err(ExecutorError::InvalidCacheConfiguration(format!(
        "task '{}' exports a secret structured output",
        command.name
      )));
    }
    let outputs = output_values
      .iter()
      .map(|path| RelativePath::new(path.clone()))
      .collect::<Result<Vec<_>, _>>()
      .map_err(|error| ExecutorError::InvalidCacheConfiguration(error.to_string()))?;
    octa_cache::validate_file_contract(&self.dir, inputs, &outputs)
      .map_err(|error| ExecutorError::InvalidCacheConfiguration(error.to_string()))?;

    for resource in command
      .task
      .artifacts
      .as_deref()
      .unwrap_or_default()
      .iter()
      .map(|artifact| artifact.path.as_path())
      .chain(
        command
          .task
          .reports
          .as_deref()
          .unwrap_or_default()
          .iter()
          .map(|report| report.path.as_path()),
      )
    {
      if resource.is_absolute() {
        return Err(ExecutorError::InvalidCacheConfiguration(format!(
          "resource '{}' must be relative to its task working directory",
          resource.display()
        )));
      }
      let absolute = self.task_working_dir(command).join(resource);
      let relative = absolute.strip_prefix(&self.dir).map_err(|_| {
        ExecutorError::InvalidCacheConfiguration(format!(
          "resource '{}' is outside the cache workspace",
          resource.display()
        ))
      })?;
      let relative = RelativePath::new(relative.to_string_lossy().replace('\\', "/"))
        .map_err(|error| ExecutorError::InvalidCacheConfiguration(error.to_string()))?;
      if !outputs.iter().any(|root| relative.is_within(root)) {
        return Err(ExecutorError::InvalidCacheConfiguration(format!(
          "resource '{}' is not contained by files.outputs",
          resource.display()
        )));
      }
    }
    Ok(())
  }

  /// Captures parser-independent task semantics for the action descriptor.
  fn execution_definition(&self, command: &FindResult) -> ExecutorResult<serde_json::Value> {
    Ok(serde_json::json!({
      "task": task_identity::task_definition(&command.task)?,
      // A dedicated semantic version avoids invalidating every cache entry for
      // executor releases that cannot affect task behavior.
      "executor_semantics": crate::result_cache::EXECUTOR_CACHE_SEMANTICS_V1,
    }))
  }

  /// Rejects output roots that two unordered cache boundaries could mutate at
  /// the same time. Ordered producer/consumer tasks may reuse the same root,
  /// but parallel ownership would make both execution and capture racy.
  pub(super) fn validate_parallel_cache_outputs(&self, dag: &DagNode) -> ExecutorResult<()> {
    let boundaries = dag
      .nodes()
      .iter()
      .filter_map(|node| {
        node
          .cache_output_contract()
          .map(|(workspace, outputs)| (node.as_ref(), workspace, outputs))
      })
      .collect::<Vec<_>>();
    let reachable = boundaries
      .iter()
      .map(|(node, _, _)| (node.id.clone(), reachable_from(dag, &node.id)))
      .collect::<HashMap<_, _>>();

    for (index, (left, left_workspace, left_outputs)) in boundaries.iter().enumerate() {
      for (right, right_workspace, right_outputs) in boundaries.iter().skip(index + 1) {
        let ordered = reachable[&left.id].contains(&right.id) || reachable[&right.id].contains(&left.id);
        if ordered || left_workspace != right_workspace {
          continue;
        }
        if let Some((left_root, right_root)) = overlapping_roots(left_outputs, right_outputs) {
          return Err(ExecutorError::InvalidCacheConfiguration(format!(
            "parallel cacheable tasks '{}' and '{}' have overlapping outputs '{}' and '{}'",
            left.dep_name, right.dep_name, left_root, right_root
          )));
        }
      }
    }
    Ok(())
  }
}

fn reachable_from(dag: &DagNode, start: &str) -> HashSet<String> {
  let mut reachable = HashSet::new();
  let mut pending = vec![start.to_owned()];
  while let Some(node) = pending.pop() {
    if let Some(children) = dag.edges().get(&node) {
      for child in children {
        if reachable.insert(child.id.clone()) {
          pending.push(child.id.clone());
        }
      }
    }
  }
  reachable
}

fn overlapping_roots<'a>(
  left: &'a [RelativePath],
  right: &'a [RelativePath],
) -> Option<(&'a RelativePath, &'a RelativePath)> {
  left.iter().find_map(|left| {
    right
      .iter()
      .find(|right| left.is_within(right) || right.is_within(left))
      .map(|right| (left, right))
  })
}

fn task_plugin_keys(task: &Task) -> Vec<String> {
  let mut keys = task.plugin.iter().map(|plugin| plugin.key.clone()).collect::<Vec<_>>();
  if let Some(conditions) = &task.condition {
    keys.extend(
      conditions
        .before_deps
        .iter()
        .chain(conditions.after_deps.iter())
        .map(|condition| condition.command.key.clone()),
    );
  }
  keys.extend(task.cmds.as_deref().unwrap_or_default().iter().flat_map(|command| {
    let plugin = match &command.payload {
      CommandPayload::Plugin(plugin) => Some(plugin.key.clone()),
      CommandPayload::Task(_) => None,
    };
    plugin
      .into_iter()
      .chain(command.options.condition.iter().map(|condition| condition.key.clone()))
  }));
  keys.sort();
  keys.dedup();
  keys
}
