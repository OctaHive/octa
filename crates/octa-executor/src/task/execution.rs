//! Runtime execution pipeline for a configured task node.
//!
//! The planner has already reduced task syntax to a [`TaskNode`] and its
//! [`NodeAction`]. Execution then follows one ordered pipeline: inherited gate
//! decisions, graph-only action, invocation context resolution, command
//! conditions, preconditions, invocation reuse, and finally plugin invocation.
//! Keeping that order here is important: work skipped by an ancestor condition
//! must not prompt for variables or execute shell-backed environment values.

use super::*;

/// Per-call values needed only by hidden graph actions.
///
/// Keeping these outside [`TaskRuntime`] leaves unrelated terminal and cache
/// services out of graph-action code; required evaluator/cache services
/// are passed explicitly to `execute_graph_action`.
struct GraphActionRuntime<'a> {
  /// Whether filesystem-changing work should be suppressed.
  dry: bool,
  /// Execution-local cooperative cancellation.
  cancel_token: &'a CancellationToken,
}

impl TaskNode {
  /// Returns the output ownership declared by this invocation's cache lookup.
  ///
  /// Only the lookup node represents the boundary, so planner validation sees
  /// each cacheable invocation exactly once rather than once per command.
  pub(crate) fn cache_output_contract(&self) -> Option<(&Path, &[octa_cache_protocol::RelativePath])> {
    match &self.action {
      NodeAction::CacheLookup { plan, .. } => Some((&plan.workspace, &plan.outputs)),
      _ => None,
    }
  }

  /// Ensures a deferred status is present on the node-owned context snapshot.
  ///
  /// Deferred plans own their invocation runtime and use one exit code for all
  /// of its nodes. The additional insertion also covers an already initialized
  /// context without mutating the stored snapshot.
  fn expose_deferred_exit_code(vars: &mut Vars, exit_code: Option<i32>) {
    if let Some(exit_code) = exit_code {
      vars.insert("EXIT_CODE", &exit_code);
    }
  }

  /// Creates an immutable executable node from planner-owned configuration.
  ///
  /// Planner-built nodes share an [`InvocationRuntime`] across conditions and
  /// commands. The fallback keeps directly constructed internal/test nodes
  /// functional without duplicating resolution logic.
  pub(crate) fn new(config: TaskConfig) -> Self {
    let invocation_runtime = config.invocation_runtime.unwrap_or_else(|| {
      Arc::new(InvocationRuntime::new(
        config.vars.clone(),
        EnvironmentPlan::from_envs(&config.envs),
        HashSet::new(),
        None,
      ))
    });
    Self {
      id: config.id,
      name: config.name,
      dep_name: config.dep_name,
      cache_key: config.cache_key,
      run_mode: config.run_mode,
      #[cfg(test)]
      vars: config.vars,
      #[cfg(test)]
      envs: config.envs,
      invocation_runtime,
      dir: config.dir,
      workspace: config.workspace,
      ignore_errors: config.ignore_errors,
      silence: config.silence,
      quiet: config.quiet,
      raw: config.raw,
      interactive_session: config.interactive_session,
      failfast: config.failfast,
      deps_res: Arc::new(Mutex::new(HashMap::default())),
      action: config.action,
      condition_runtime: config.condition_runtime,
      task_cache_runtime: config.task_cache_runtime,
      preconditions: config.preconditions,
      timeout: config.timeout,
      execution_binding: config.execution_binding,
      prefix_template: config.prefix_template,
      step_exports: config.step_exports,
      plugin: config.plugin,
    }
  }

  #[cfg(test)]
  /// Returns normalized condition commands for planner assertions.
  pub(crate) fn conditions(&self) -> Vec<String> {
    self
      .condition_runtime
      .conditions()
      .iter()
      .map(PluginInvocation::command)
      .collect()
  }

  #[cfg(test)]
  /// Returns the task scope bound to this node, if any.
  pub(crate) fn output_scope(&self) -> Option<&ConsoleScope> {
    self.execution_binding.as_ref().map(ExecutionBinding::scope)
  }

  #[cfg(test)]
  /// Returns the shared interactive-session identity used by this node.
  pub(crate) fn interactive_session(&self) -> Option<&str> {
    self.interactive_session.as_deref()
  }

  /// Renders a potentially templated task directory from resolved variables.
  ///
  /// Plugin-backed template helpers execute from the host's current directory
  /// because the rendered task directory may not exist until rendering ends.
  async fn interpolate_dir(
    &self,
    dir: PathBuf,
    vars: &Vars,
    evaluator: Option<Arc<dyn PluginEvaluator>>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<PathBuf> {
    let dir_str = dir.to_string_lossy();

    if !dir_str.contains("{{") || !dir_str.contains("}}") {
      debug!("Using direct directory path: {}", dir_str);

      Ok(dir)
    } else {
      debug!("Expanding directory path: {}", dir_str);

      let context: Context = vars.clone().into();
      let renderer = TemplateRenderer::new(
        context,
        PluginTemplateContext::new(
          evaluator,
          PluginExecutionContext {
            // The rendered task directory does not exist yet, so helpers execute from its base.
            dir: env::current_dir()?,
            vars: vars.to_hashmap(),
            envs: HashMap::new(),
            secret_vars: vars.secret_names(),
            dry,
            redact_params: false,
          },
          cancel_token,
        ),
      );
      let rendered = renderer
        .render(dir_str.to_string())
        .await
        .map_err(|error| ExecutorError::ValueExpandError(dir_str.to_string(), error))?;

      debug!("Expanded path: {}", rendered);

      // Tera may serialize a string-valued expression with surrounding quotes;
      // directory configuration expects the underlying path.
      Ok(PathBuf::from(rendered.trim_matches('"')))
    }
  }

  #[cfg(test)]
  /// Test seam for directory interpolation without a plugin evaluator.
  pub(super) async fn prepare_dir_with_vars(&self, vars: &Vars, dry: bool) -> ExecutorResult<PathBuf> {
    let dir = self
      .interpolate_dir(self.dir.clone(), vars, None, dry, CancellationToken::new())
      .await?;

    self.ensure_dir(dir, dry).await
  }

  /// Resolves and prepares the working directory used during real execution.
  async fn prepare_runtime_dir(
    &self,
    vars: &Vars,
    evaluator: Arc<dyn PluginEvaluator>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<PathBuf> {
    let dir = self
      .interpolate_dir(self.dir.clone(), vars, Some(evaluator), dry, cancel_token)
      .await?;

    self.ensure_dir(dir, dry).await
  }

  /// Returns a canonical execution directory, creating it outside dry-run mode.
  ///
  /// Dry runs preserve missing absolute paths and anchor missing relative paths
  /// without mutating the filesystem. Existing paths are still canonicalized so
  /// template/plugin behavior matches a normal execution.
  async fn ensure_dir(&self, dir: PathBuf, dry: bool) -> ExecutorResult<PathBuf> {
    if dry {
      return match canonicalize(&dir) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
          if dir.is_absolute() {
            Ok(dir)
          } else {
            Ok(env::current_dir()?.join(dir))
          }
        },
        Err(error) => Err(error.into()),
      };
    }

    tokio::fs::create_dir_all(&dir).await?;
    Ok(canonicalize(dir)?)
  }

  /// Resolves the values shared by conditions, cache checks, and the task command once.
  ///
  /// The `OnceCell` belongs to the complete task invocation rather than an
  /// individual DAG node. Required prompts, variable shell commands, and
  /// environment shell commands therefore execute at most once even when the
  /// invocation contains several condition and command nodes.
  pub(super) async fn resolve_runtime_context(
    &self,
    evaluator: Arc<dyn PluginEvaluator>,
    dry: bool,
    cancel_token: CancellationToken,
    deferred_exit_code: Option<i32>,
  ) -> ExecutorResult<RuntimeContext> {
    let context = self
      .invocation_runtime
      .context
      .get_or_try_init(|| async {
        // Dependency values can be large JSON objects. Resolve and clone them
        // only for the node that initializes the invocation-wide context.
        let mut overrides = {
          let dependencies = self.deps_res.lock().await;
          self.task_output_overrides(&dependencies)?
        };
        if let Some(exit_code) = deferred_exit_code {
          // This must precede expansion so deferred task vars and environments
          // can reference EXIT_CODE in their templates.
          overrides.insert("EXIT_CODE".to_owned(), Value::from(exit_code), false);
        }
        let dir_is_template = {
          let value = self.dir.to_string_lossy();
          value.contains("{{") && value.contains("}}")
        };

        // Static task directories must exist before a shell-backed value uses them as its cwd.
        let prepared_dir = if dir_is_template {
          None
        } else {
          Some(self.ensure_dir(self.dir.clone(), dry).await?)
        };

        // Required input is resolved before template expansion so supplied
        // values are available to every later variable and directory template.
        let mut vars = self.invocation_runtime.vars.clone();
        vars
          .resolve_required_with_overrides(self.invocation_runtime.resolver.as_deref(), &overrides)
          .await?;
        vars
          .expand_with_evaluator_and_overrides(evaluator.clone(), dry, cancel_token.clone(), overrides)
          .await?;

        // A templated directory can only be created after its variables have been expanded.
        let dir = match prepared_dir {
          Some(dir) => dir,
          None => {
            self
              .prepare_runtime_dir(&vars, evaluator.clone(), dry, cancel_token.clone())
              .await?
          },
        };

        let mut environment = self.invocation_runtime.environment.clone();
        // Task-level shell-backed environment values must run from the final task directory.
        environment.set_last_dir(dir.clone());
        let envs = environment.resolve(&vars, Some(evaluator), dry, cancel_token).await?;

        Ok::<RuntimeContext, ExecutorError>(RuntimeContext {
          vars,
          envs,
          dir,
          identity_names: self.invocation_runtime.identity_names.clone(),
          plugin_uses: self.invocation_runtime.plugin_uses.clone(),
        })
      })
      .await?;

    // Consumers receive an owned snapshot; task-specific additions cannot
    // mutate the invocation-wide cached context.
    let mut context = context.clone();
    Self::expose_deferred_exit_code(&mut context.vars, deferred_exit_code);
    Ok(context)
  }

  /// Resolves dependency references at the task boundary, keeping [`Vars`] independent of DAG transport types.
  fn task_output_overrides(
    &self,
    dependencies: &HashMap<String, DependencyResult>,
  ) -> ExecutorResult<VariableOverrides> {
    let mut overrides = VariableOverrides::default();
    for variable in self.invocation_runtime.vars.task_output_references() {
      let output = dependencies
        .get(&variable.reference.task)
        .and_then(|result| result.outputs().get(&variable.reference.output))
        .cloned()
        .ok_or_else(|| ExecutorError::DependencyOutputMissing {
          variable: variable.name.clone(),
          task: variable.reference.task.clone(),
          output: variable.reference.output.clone(),
        })?;
      let inherited_secret = dependencies
        .get(&variable.reference.task)
        .is_some_and(|result| result.outputs().is_secret(&variable.reference.output));
      overrides.insert(variable.name, output, variable.secret || inherited_secret);
    }
    Ok(overrides)
  }

  /// Emits informational task messages only for visible command nodes.
  ///
  /// Hidden condition/cache/barrier nodes must not create user-facing
  /// "Starting task" noise, and quiet mode suppresses the remaining messages.
  async fn log_info(&self, output: &RuntimeOutput, message: String) -> ExecutorResult<()> {
    if self.action.is_command() && !self.quiet {
      output.message(ConsoleLevel::Info, message).await?;
    }
    Ok(())
  }

  /// Executes graph-only actions before entering the plugin command path.
  ///
  /// `Some` is a complete node outcome. `None` means the node still needs the
  /// common condition/precondition/plugin pipeline below.
  async fn execute_graph_action(
    &self,
    result_cache: Option<&crate::result_cache::ResultCache>,
    output: &RuntimeOutput,
    runtime: GraphActionRuntime<'_>,
  ) -> ExecutorResult<Option<TaskOutcome>> {
    match &self.action {
      // Condition nodes use the common plugin-condition path. Command nodes
      // continue all the way to their configured plugin invocation.
      NodeAction::Command
      | NodeAction::Condition
      | NodeAction::CacheLookup { .. }
      | NodeAction::RegisterResources { .. } => Ok(None),
      // Barrier nodes only preserve graph ordering and carry no runtime payload.
      NodeAction::Barrier => Ok(Some(TaskOutcome::success(String::new()))),
      NodeAction::CacheFinalize { plan, state } => {
        let Some(cache) = result_cache else {
          return Err(ExecutorError::InvalidCacheConfiguration(
            "task enables caching but no result cache was configured".to_owned(),
          ));
        };
        let (stdout, outputs, _) =
          crate::result_cache::finalize(cache, plan, state, output, runtime.cancel_token, runtime.dry).await?;
        Ok(Some(TaskOutcome::success(stdout).with_outputs(outputs)))
      },
    }
  }

  /// Evaluates every plugin-backed condition attached to this node.
  ///
  /// Conditions receive dependency results under `deps_result` but do not emit
  /// normal task output. A non-zero plugin status means "false"; transport and
  /// protocol errors remain execution failures.
  async fn check_condition(
    &self,
    plugin_manager: Arc<PluginManager>,
    dry: bool,
    cancel_token: CancellationToken,
    vars: &Vars,
    envs: &Envs,
    dir: &Path,
  ) -> ExecutorResult<bool> {
    if self.condition_runtime.conditions().is_empty() {
      return Ok(true);
    }
    if dry {
      return Ok(true);
    }

    // Copy references into the template/plugin value before releasing the lock;
    // no plugin call is allowed to hold scheduler result state across an await.
    let deps_res = self.deps_res.lock().await;
    let mut vars = vars.clone();
    let dependency_values = deps_res
      .iter()
      .map(|(name, value)| (name.as_str(), value.stdout()))
      .collect::<HashMap<_, _>>();
    vars.insert("deps_result", &dependency_values);
    drop(deps_res);

    let invoker = PluginInvoker::new(plugin_manager);
    // Conditions are conjunctive and preserve their configured order. Stop on
    // the first false result to avoid unnecessary plugin work.
    for condition in self.condition_runtime.conditions() {
      let request = PluginRequest {
        target: crate::plugin::PluginTarget::Key(condition.key.clone()),
        value: condition.value(),
        args: vec![],
        context: PluginExecutionContext {
          dir: dir.to_path_buf(),
          vars: vars.to_hashmap(),
          envs: envs.clone().into(),
          secret_vars: vars.secret_names(),
          dry,
          redact_params: false,
        },
        output: None,
        raw: false,
      };
      match invoker.invoke(request, cancel_token.clone()).await {
        Ok(output) if output.code == 0 => {},
        Ok(_) => return Ok(false),
        Err(ExecutorError::IoError(error)) if error.kind() == io::ErrorKind::Interrupted => {
          return Err(ExecutorError::TaskCancelled(self.name.clone()));
        },
        Err(error) => return Err(error),
      }
    }

    Ok(true)
  }

  /// Renders task preconditions and requires every value to be truthy.
  ///
  /// Preconditions use Tera/plugin helpers and dependency results from the same
  /// resolved context as the command. They differ from conditions by treating a
  /// false value as task cancellation/failure rather than a clean skip.
  async fn check_preconditions(
    &self,
    evaluator: Arc<dyn PluginEvaluator>,
    vars: &Vars,
    envs: &Envs,
    dir: &Path,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<bool> {
    let Some(preconditions) = &self.preconditions else {
      return Ok(true);
    };

    let mut context: Context = vars.clone().into();
    // Dependency results are input data, not top-level task variables, so keep
    // them under the dedicated `deps_result` namespace.
    let deps_res = self.deps_res.lock().await;
    let dependency_values = deps_res
      .iter()
      .map(|(name, value)| (name.as_str(), value.stdout()))
      .collect::<HashMap<_, _>>();
    context.insert("deps_result", &dependency_values);
    drop(deps_res);
    let renderer = TemplateRenderer::new(
      context,
      PluginTemplateContext::new(
        Some(evaluator),
        PluginExecutionContext {
          dir: dir.to_path_buf(),
          vars: vars.to_hashmap(),
          envs: envs.clone().into(),
          secret_vars: vars.secret_names(),
          dry,
          redact_params: false,
        },
        cancel_token,
      ),
    );

    let mut result = true;

    for precondition in preconditions {
      let rendered = renderer
        .render(precondition)
        .await
        .map_err(|error| ExecutorError::ValueExpandError(precondition.to_owned(), error))?;

      // Render every precondition to surface configuration errors consistently,
      // even after an earlier expression evaluated to false.
      result = result && (rendered.trim() == "true" || rendered.trim() == "True" || rendered.trim() == "1");
    }

    Ok(result)
  }

  /// Returns a reusable result according to the node's configured run mode.
  ///
  /// `once` ignores variable changes, while `changed` reuses output only for an
  /// equal resolved variable set. `always` never consults the cache.
  async fn check_invocation_result(
    &self,
    vars: &Vars,
    results: &Arc<Mutex<IndexMap<String, InvocationResult>>>,
  ) -> ExecutorResult<Option<InvocationResult>> {
    if self.run_mode == RunMode::Always {
      return Ok(None);
    }

    let results = results.lock().await;
    if let Some(cached_result) = results.get(&self.cache_key) {
      if self.run_mode == RunMode::Once {
        return Ok(Some(cached_result.clone()));
      } else if &cached_result.vars == vars {
        debug!("Invocation result reused for task: {}", self.name);
        return Ok(Some(cached_result.clone()));
      }
    }
    Ok(None)
  }

  /// Stores a successful result for `once` and `changed` run modes.
  async fn update_invocation_result(
    &self,
    result: &str,
    vars: &Vars,
    outputs: &CompletionOutputs,
    results: &Arc<Mutex<IndexMap<String, InvocationResult>>>,
  ) -> ExecutorResult<()> {
    if self.run_mode != RunMode::Always {
      let mut results = results.lock().await;
      results.insert(
        self.cache_key.clone(),
        InvocationResult::new(result.to_string(), vars.clone(), outputs.clone()),
      );
      debug!("Stored invocation result for task: {}", self.name);
    }
    Ok(())
  }

  /// Logs dependency names without cloning or exposing their potentially sensitive values.
  async fn debug_log_dependencies(&self) {
    if enabled!(Level::DEBUG) {
      let deps = self.deps_res.lock().await;
      debug!(dependencies = ?deps.keys().collect::<Vec<_>>(), "Resolved dependency results");
    }
  }

  /// Selects task-level outputs and removes secret exports from the public step result.
  pub(super) fn completion_outputs(
    &self,
    mut step_outputs: Map<String, Value>,
    artifacts: &[octa_plugin::protocol::ArtifactDeclaration],
    reports: &[octa_plugin::protocol::ReportDeclaration],
    working_dir: &Path,
  ) -> ExecutorResult<CompletionOutputs> {
    let mut task_outputs = TaskOutputs::default();
    for (name, export) in &self.step_exports {
      let value = step_outputs
        .get(&export.field)
        .cloned()
        .ok_or_else(|| ExecutorError::TaskOutputMissing {
          step: self
            .execution_binding
            .as_ref()
            .and_then(|binding| binding.step())
            .map_or_else(|| self.name.clone(), |step| step.label().to_owned()),
          field: export.field.clone(),
        })?;
      task_outputs.insert(name.clone(), value, export.secret);
      if export.secret {
        step_outputs.remove(&export.field);
      }
    }
    let artifacts = crate::resource::plugin_artifacts(artifacts, working_dir, &self.workspace)?;
    let reports = crate::resource::plugin_reports(reports, working_dir, &self.workspace)?;
    Ok(CompletionOutputs::new(step_outputs, task_outputs).with_resources(artifacts, reports))
  }

  /// Executes the task without applying its timeout wrapper.
  ///
  /// This is the canonical node pipeline. Early returns are deliberately
  /// ordered from cheapest/shared decisions to operations that may prompt,
  /// touch the filesystem, or invoke a plugin.
  async fn execute_inner(&self, runtime: TaskRuntime, cancel_token: CancellationToken) -> ExecutorResult<TaskOutcome> {
    let TaskRuntime {
      plugin_manager,
      terminal,
      invocation_results,
      result_cache,
      console,
      run_id,
      dry,
      force,
      cache_probe,
      deferred_exit_code,
      structured_output_budget,
    } = runtime;
    let console_target = RuntimeOutput::with_silence(console, run_id, self.execution_binding.clone(), self.silence);
    let evaluator: Arc<dyn PluginEvaluator> = Arc::new(ManagerPluginEvaluator::tracking(
      plugin_manager.clone(),
      self.invocation_runtime.plugin_uses.clone(),
    ));

    // Inherited gates are checked before runtime-context resolution. This is
    // what prevents required-variable prompts for tasks already known to skip.
    if !self.condition_runtime.should_run(&self.name)? {
      return Ok(TaskOutcome::skipped(String::new()));
    }

    // Cache guards precede graph actions so an outer hit suppresses nested
    // lookup/finalize nodes as well as ordinary commands.
    if !self.task_cache_runtime.should_run().await {
      return Ok(TaskOutcome::skipped(String::new()));
    }

    // Barriers and cache finalizers can finish without command setup.
    if let Some(result) = self
      .execute_graph_action(
        result_cache.as_deref(),
        &console_target,
        GraphActionRuntime {
          dry,
          cancel_token: &cancel_token,
        },
      )
      .await?
    {
      return Ok(result);
    }

    // From this point on every check and the command itself shares one resolved
    // variables/environment/directory snapshot.
    let RuntimeContext {
      vars,
      envs,
      dir,
      identity_names,
      plugin_uses,
    } = self
      .resolve_runtime_context(evaluator.clone(), dry, cancel_token.clone(), deferred_exit_code)
      .await?;
    // Output prefix templates are runtime metadata on the scope. Resolve them
    // once here so renderers never need access to executor variables.
    if let Some(scope) = self.execution_binding.as_ref().map(ExecutionBinding::scope) {
      let mut values = vars.to_merged_hashmap();
      values.insert("TASK".to_owned(), serde_json::Value::String(self.name.clone()));
      scope.set_template_values(values.clone());
      if let Some(template) = &self.prefix_template {
        let prefix = octa_output::render_output_template(template, &values)
          .map_err(|error| ExecutorError::ValueExpandError(template.clone(), error.to_string()))?;
        scope.set_prefix(Some(prefix));
      }
    }
    let condition_passed = self
      .check_condition(
        plugin_manager.clone(),
        dry || cache_probe,
        cancel_token.clone(),
        &vars,
        &envs,
        &dir,
      )
      .await?;
    self.condition_runtime.publish(condition_passed);
    if !condition_passed {
      self
        .log_info(
          &console_target,
          format!("Task '{}' skipped because its condition was not met", self.name),
        )
        .await?;
      return Ok(TaskOutcome::skipped(String::new()));
    }

    // `force` bypasses precondition shortcuts and persistent cache lookup;
    // cancellation still remains active.
    if !force
      && !self
        .check_preconditions(evaluator, &vars, &envs, &dir, dry || cache_probe, cancel_token.clone())
        .await?
    {
      self
        .log_info(&console_target, format!("Task '{}' preconditions failed", self.name))
        .await?;

      return Err(ExecutorError::TaskCancelled(format!(
        "Task '{}' preconditions failed",
        self.name
      )));
    }

    if let NodeAction::CacheLookup { plan, state } = &self.action {
      let Some(cache) = result_cache.as_deref() else {
        return Err(ExecutorError::InvalidCacheConfiguration(
          "task enables caching but no result cache was configured".to_owned(),
        ));
      };
      state.set_probe(cache_probe).await;
      crate::result_cache::lookup(
        cache,
        crate::result_cache::CacheLookup {
          plan,
          state,
          plugin_manager: &plugin_manager,
          context: &RuntimeContext {
            vars,
            envs,
            dir,
            identity_names,
            plugin_uses,
          },
          output: &console_target,
          cancel: &cancel_token,
          dry,
          force,
        },
      )
      .await?;
      return Ok(TaskOutcome::success(String::new()));
    }

    if let NodeAction::RegisterResources { artifacts, reports } = &self.action {
      if cache_probe {
        return Ok(TaskOutcome::skipped(String::new()));
      }
      let artifacts = crate::resource::octafile_artifacts(artifacts, &self.dir, &self.workspace)?;
      let reports = crate::resource::octafile_reports(reports, &self.dir, &self.workspace)?;
      let outputs = CompletionOutputs::default().with_resources(artifacts, reports);
      self.task_cache_runtime.record("", &outputs).await;
      return Ok(TaskOutcome::success(String::new()).with_outputs(outputs));
    }

    if cache_probe {
      return Ok(TaskOutcome::skipped(String::new()));
    }

    if let Some(cached) = self.check_invocation_result(&vars, &invocation_results).await? {
      structured_output_budget.reserve(&cached.outputs)?;
      return Ok(TaskOutcome::skipped(cached.result).with_outputs(cached.outputs));
    }

    self
      .log_info(&console_target, format!("Starting task {}", self.name))
      .await?;
    self.debug_log_dependencies().await;

    // Condition nodes have no command payload: after publishing their decision
    // they terminate successfully here.
    let Some(plugin) = &self.plugin else {
      return Ok(TaskOutcome::success(String::new()));
    };
    // Dependency results are added only to the command snapshot, never to the
    // shared invocation variables cached above.
    let mut vars_with_deps_results = vars.clone();
    let deps_res = self.deps_res.lock().await;
    let dependency_values = deps_res
      .iter()
      .map(|(name, value)| (name.as_str(), value.stdout()))
      .collect::<HashMap<_, _>>();
    vars_with_deps_results.insert("deps_result", &dependency_values);
    drop(deps_res);

    let working_dir = dir.clone();
    let request = PluginRequest {
      target: crate::plugin::PluginTarget::Key(plugin.key.clone()),
      value: plugin.value(),
      args: vec![],
      context: PluginExecutionContext {
        dir,
        vars: vars_with_deps_results.to_hashmap(),
        envs: envs.into(),
        secret_vars: vars_with_deps_results.secret_names(),
        dry,
        redact_params: false,
      },
      output: Some(console_target.clone()),
      raw: self.raw,
    };
    let (result, outputs) = match PluginInvoker::with_terminal(plugin_manager, terminal)
      .invoke(request, cancel_token.clone())
      .await
    {
      Ok(output) => {
        let PluginOutput {
          code,
          stdout,
          stderr,
          outputs,
          artifacts,
          reports,
          failure_location,
        } = output;
        if code != 0 && !cancel_token.is_cancelled() {
          if self.ignore_errors {
            // Ignored failures are terminal successes with no structured
            // result. Failed completions are not output-schema validated, so
            // neither step results nor dependency exports may retain them.
            console_target
              .message(
                ConsoleLevel::Error,
                format!("Task {} failed but errors ignored. Error code: {}", self.name, code),
              )
              .await?;
            (Ok("".to_string()), CompletionOutputs::default())
          } else {
            (
              Err(ExecutorError::CommandFailed {
                task: self.name.clone(),
                code,
                stderr,
                location: failure_location,
              }),
              Default::default(),
            )
          }
        } else {
          // Only successful command output participates in invocation reuse and
          // persistent cache capture.
          let outputs = self.completion_outputs(outputs, &artifacts, &reports, &working_dir)?;
          structured_output_budget.reserve(&outputs)?;
          self
            .update_invocation_result(stdout.trim(), &vars, &outputs, &invocation_results)
            .await?;
          self.task_cache_runtime.record(stdout.trim(), &outputs).await;
          (Ok(stdout.trim().to_string()), outputs)
        }
      },
      Err(ExecutorError::IoError(error)) if error.kind() == io::ErrorKind::Interrupted => {
        (Err(ExecutorError::TaskCancelled(self.name.clone())), Default::default())
      },
      Err(error) => (
        self.handle_execution_error(&console_target, error).await,
        Default::default(),
      ),
    };
    result.map(|output| TaskOutcome::success(output).with_outputs(outputs))
  }

  /// Converts an invocation error according to the task's `ignore_error` policy.
  async fn handle_execution_error(&self, output: &RuntimeOutput, error: ExecutorError) -> ExecutorResult<String> {
    if self.ignore_errors {
      output
        .message(
          ConsoleLevel::Error,
          format!("Task {} failed but errors ignored. Error: {error}", self.name),
        )
        .await?;
      Ok("".to_string())
    } else {
      Err(error)
    }
  }
}

#[async_trait]
impl Executable for TaskNode {
  /// Stores one direct dependency result for later templates and plugin input.
  async fn set_result(&self, task_name: String, result: DependencyResult) {
    let mut deps_res = self.deps_res.lock().await;
    match deps_res.get_mut(&task_name) {
      Some(existing) => existing.merge(result),
      None => {
        deps_res.insert(task_name, result);
      },
    }
  }

  /// Replaces dependency results when an internal node propagates a bypassed branch.
  async fn bypass_result(&self, result: HashMap<String, DependencyResult>) {
    let mut deps_res = self.deps_res.lock().await;
    for (name, result) in result {
      match deps_res.get_mut(&name) {
        Some(existing) => existing.merge(result),
        None => {
          deps_res.insert(name, result);
        },
      }
    }
  }

  /// Executes the node and enforces its optional wall-clock timeout.
  ///
  /// Timeout cancellation uses a child token so it stops only this command. The
  /// executor waits briefly for protocol cleanup before returning, allowing the
  /// same long-lived plugin connection to accept subsequent commands safely.
  async fn execute(&self, runtime: TaskRuntime, cancel_token: CancellationToken) -> ExecutorResult<TaskOutcome> {
    let Some(timeout) = self.timeout else {
      return self.execute_inner(runtime, cancel_token).await;
    };

    // A child token limits cancellation to this command while preserving the caller's token.
    let command_token = cancel_token.child_token();
    let output = RuntimeOutput::with_silence(
      runtime.console.clone(),
      runtime.run_id,
      self.execution_binding.clone(),
      self.silence,
    );
    let execution = self.execute_inner(runtime, command_token.clone());
    tokio::pin!(execution);

    tokio::select! {
      result = &mut execution => result,
      _ = time::sleep(timeout.duration()) => {
        command_token.cancel();
        // Wait for protocol cleanup so the plugin can accept another command immediately.
        let _ = time::timeout(Duration::from_secs(5), &mut execution).await;
        let error = ExecutorError::TaskTimedOut {
          task: self.name.clone(),
          timeout: timeout.to_string(),
        };
        if self.ignore_errors {
          output
            .message(
              ConsoleLevel::Error,
              format!("Task {} failed but errors ignored. Error: {error}", self.name),
            )
            .await?;
          Ok(TaskOutcome::success(String::new()))
        } else {
          Err(error)
        }
      }
    }
  }
}
