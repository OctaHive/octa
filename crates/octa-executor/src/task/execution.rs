//! Runtime execution pipeline for a configured task node.
//!
//! The planner has already reduced task syntax to a [`TaskNode`] and its
//! [`NodeAction`]. Execution then follows one ordered pipeline: inherited gate
//! decisions, graph-only action, invocation context resolution, command
//! conditions, preconditions, run-mode cache, and finally plugin invocation.
//! Keeping that order here is important: work skipped by an ancestor condition
//! must not prompt for variables or execute shell-backed environment values.

use super::*;

/// Per-call values needed only by hidden graph actions.
///
/// Keeping these outside [`TaskRuntime`] leaves unrelated terminal and cache
/// services out of graph-action code; required evaluator/fingerprint services
/// are passed explicitly to `execute_graph_action`.
struct GraphActionRuntime<'a> {
  /// Whether filesystem-changing work should be suppressed.
  dry: bool,
  /// Whether freshness should be treated as stale.
  force: bool,
  /// Execution-local cooperative cancellation.
  cancel_token: &'a CancellationToken,
  /// Exit status exposed while executing a deferred plan.
  deferred_exit_code: Option<i32>,
}

impl TaskNode {
  /// Builds highest-priority expansion overrides for a deferred invocation.
  ///
  /// `EXIT_CODE` must be visible while templates and shell-backed values are
  /// resolved, not merely added to the final plugin variable map.
  fn deferred_variable_overrides(exit_code: Option<i32>) -> IndexMap<String, Value> {
    exit_code
      .map(|exit_code| ("EXIT_CODE".to_owned(), Value::from(exit_code)))
      .into_iter()
      .collect()
  }

  /// Adds `EXIT_CODE` to a cloned resolved context without polluting the shared cache.
  ///
  /// Invocation contexts are cached in a `OnceCell`. Applying this value after
  /// cloning prevents one deferred execution's status from becoming permanent
  /// state for another consumer of that context.
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
      run_mode: config.run_mode,
      #[cfg(test)]
      vars: config.vars,
      #[cfg(test)]
      envs: config.envs,
      invocation_runtime,
      dir: config.dir,
      ignore_errors: config.ignore_errors,
      silence: config.silence,
      quiet: config.quiet,
      raw: config.raw,
      interactive_session: config.interactive_session,
      failfast: config.failfast,
      deps_res: Arc::new(Mutex::new(HashMap::default())),
      action: config.action,
      condition_runtime: config.condition_runtime,
      freshness_runtime: config.freshness_runtime,
      preconditions: config.preconditions,
      timeout: config.timeout,
      execution_binding: config.execution_binding,
      prefix_template: config.prefix_template,
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

  /// Returns watch metadata only from the node that owns freshness evaluation.
  ///
  /// Command and commit nodes deliberately return nothing so one invocation
  /// cannot register the same source tree multiple times.
  pub(crate) fn watch_target(&self) -> Option<WatchTarget> {
    match &self.action {
      NodeAction::FreshnessCheck { spec, .. } => spec.watch_target(),
      _ => None,
    }
  }

  #[cfg(test)]
  /// Exposes the selected freshness method for planner tests.
  pub(crate) fn source_method(&self) -> Option<SourceMethod> {
    match &self.action {
      NodeAction::FreshnessCheck { spec, .. } => Some(spec.method()),
      _ => None,
    }
  }

  #[cfg(test)]
  /// Exposes the concrete source-strategy key for planner tests.
  pub(crate) fn source_strategy_key(&self) -> Option<&'static str> {
    match &self.action {
      NodeAction::FreshnessCheck { spec, .. } => Some(spec.strategy_key()),
      _ => None,
    }
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
          .resolve_required(self.invocation_runtime.resolver.as_deref())
          .await?;
        vars
          .expand_with_evaluator_and_overrides(
            evaluator.clone(),
            dry,
            cancel_token.clone(),
            Self::deferred_variable_overrides(deferred_exit_code),
          )
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

        Ok::<RuntimeContext, ExecutorError>(RuntimeContext { vars, envs, dir })
      })
      .await?;

    // Consumers receive an owned snapshot; task-specific additions cannot
    // mutate the invocation-wide cached context.
    let mut context = context.clone();
    Self::expose_deferred_exit_code(&mut context.vars, deferred_exit_code);
    Ok(context)
  }

  /// Emits informational task messages only for visible command nodes.
  ///
  /// Hidden condition/freshness/barrier nodes must not create user-facing
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
    fingerprint: &Db,
    evaluator: Arc<dyn PluginEvaluator>,
    output: &RuntimeOutput,
    runtime: GraphActionRuntime<'_>,
  ) -> ExecutorResult<Option<TaskOutcome>> {
    if !self.freshness_runtime.should_run()? {
      // A skipped condition gate must still publish a result so descendants do not observe an
      // unevaluated gate when an ancestor freshness check suppresses the whole invocation.
      self.condition_runtime.publish(false);
      if let NodeAction::FreshnessCheck { state, .. } = &self.action {
        state.publish_skipped()?;
      }
      return Ok(Some(TaskOutcome::skipped(String::new())));
    }

    match &self.action {
      // Condition nodes use the common plugin-condition path. Command nodes
      // continue all the way to their configured plugin invocation.
      NodeAction::Command | NodeAction::Condition => Ok(None),
      // Barrier nodes only preserve graph ordering and carry no runtime payload.
      NodeAction::Barrier => Ok(Some(TaskOutcome::success(String::new()))),
      NodeAction::FreshnessCheck { spec, state } => {
        let runtime_context = self
          .resolve_runtime_context(
            evaluator,
            runtime.dry,
            runtime.cancel_token.clone(),
            runtime.deferred_exit_code,
          )
          .await?;
        let outcome = spec
          .evaluate(
            fingerprint,
            runtime.force,
            &runtime_context.vars,
            &runtime_context.envs,
            runtime.cancel_token,
          )
          .await?;
        // Publish before returning so every downstream node observes a fully
        // initialized decision when the scheduler releases it.
        let should_run = outcome.should_run();
        state.publish(outcome)?;
        if !should_run {
          output
            .message(ConsoleLevel::Info, format!("Task {} is up to date", self.dep_name))
            .await?;
        }
        Ok(Some(if should_run {
          TaskOutcome::success(String::new())
        } else {
          TaskOutcome::skipped(String::new())
        }))
      },
      NodeAction::FreshnessCommit(state) => {
        // Dry runs evaluate freshness for accurate planning output but never
        // persist state describing work that was not actually performed.
        if !runtime.dry {
          state.commit(fingerprint)?;
        }
        Ok(Some(TaskOutcome::success(String::new())))
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
      .map(|(name, value)| (name.as_str(), value.as_ref()))
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
      .map(|(name, value)| (name.as_str(), value.as_ref()))
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
  async fn check_cache(
    &self,
    vars: &Vars,
    cache: &Arc<Mutex<IndexMap<String, CacheItem>>>,
  ) -> ExecutorResult<Option<String>> {
    if self.run_mode == RunMode::Always {
      return Ok(None);
    }

    let cache_lock = cache.lock().await;
    if let Some(cached_result) = cache_lock.get(&self.name) {
      if self.run_mode == RunMode::Once {
        return Ok(Some(cached_result.result.clone()));
      } else if &cached_result.vars == vars {
        debug!("Cache hit for task: {}", self.name);
        return Ok(Some(cached_result.result.clone()));
      }
    }
    Ok(None)
  }

  /// Stores a successful result for `once` and `changed` run modes.
  async fn update_cache(
    &self,
    result: &str,
    vars: Vars,
    cache: &Arc<Mutex<IndexMap<String, CacheItem>>>,
  ) -> ExecutorResult<()> {
    if self.run_mode != RunMode::Always {
      let mut cache_lock = cache.lock().await;
      cache_lock.insert(self.name.clone(), CacheItem::new(result.to_string(), vars.clone()));
      debug!("Cached result for task: {}", self.name);
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

  /// Executes the task without applying its timeout wrapper.
  ///
  /// This is the canonical node pipeline. Early returns are deliberately
  /// ordered from cheapest/shared decisions to operations that may prompt,
  /// touch the filesystem, or invoke a plugin.
  async fn execute_inner(&self, runtime: TaskRuntime, cancel_token: CancellationToken) -> ExecutorResult<TaskOutcome> {
    let TaskRuntime {
      plugin_manager,
      terminal,
      cache,
      fingerprint,
      console,
      run_id,
      dry,
      force,
      deferred_exit_code,
    } = runtime;
    let console_target = RuntimeOutput::with_silence(console, run_id, self.execution_binding.clone(), self.silence);
    let evaluator: Arc<dyn PluginEvaluator> = Arc::new(ManagerPluginEvaluator::new(plugin_manager.clone()));

    // Inherited gates are checked before runtime-context resolution. This is
    // what prevents required-variable prompts for tasks already known to skip.
    if !self.condition_runtime.should_run(&self.name)? {
      if let NodeAction::FreshnessCheck { state, .. } = &self.action {
        state.publish_skipped()?;
      }
      return Ok(TaskOutcome::skipped(String::new()));
    }

    // Barriers and freshness nodes can finish without entering command setup.
    if let Some(result) = self
      .execute_graph_action(
        &fingerprint,
        evaluator.clone(),
        &console_target,
        GraphActionRuntime {
          dry,
          force,
          cancel_token: &cancel_token,
          deferred_exit_code,
        },
      )
      .await?
    {
      return Ok(result);
    }

    // From this point on every check and the command itself shares one resolved
    // variables/environment/directory snapshot.
    let RuntimeContext { vars, envs, dir } = self
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
      .check_condition(plugin_manager.clone(), dry, cancel_token.clone(), &vars, &envs, &dir)
      .await?;
    self.condition_runtime.publish(condition_passed);
    if !condition_passed {
      // A condition skip must also suppress the later freshness commit.
      self.freshness_runtime.mark_condition_skipped();
      self
        .log_info(
          &console_target,
          format!("Task '{}' skipped because its condition was not met", self.name),
        )
        .await?;
      return Ok(TaskOutcome::skipped(String::new()));
    }

    // `force` is an explicit request to run regardless of freshness and
    // precondition shortcuts; cancellation still remains active.
    if !force
      && !self
        .check_preconditions(evaluator, &vars, &envs, &dir, dry, cancel_token.clone())
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

    if let Some(cached) = self.check_cache(&vars, &cache).await? {
      return Ok(TaskOutcome::skipped(cached));
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
      .map(|(name, value)| (name.as_str(), value.as_ref()))
      .collect::<HashMap<_, _>>();
    vars_with_deps_results.insert("deps_result", &dependency_values);
    drop(deps_res);

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
    let result = match PluginInvoker::with_terminal(plugin_manager, terminal)
      .invoke(request, cancel_token.clone())
      .await
    {
      Ok(output) => {
        if output.code != 0 && !cancel_token.is_cancelled() {
          if self.ignore_errors {
            // Ignored failures are terminal successes with empty dependency
            // output, while the diagnostic remains visible to the user.
            console_target
              .message(
                ConsoleLevel::Error,
                format!(
                  "Task {} failed but errors ignored. Error code: {}",
                  self.name, output.code
                ),
              )
              .await?;
            Ok("".to_string())
          } else {
            Err(ExecutorError::CommandFailed {
              task: self.name.clone(),
              code: output.code,
              stderr: output.stderr,
              location: output.failure_location,
            })
          }
        } else {
          // Only successful command output participates in task run-mode cache.
          self.update_cache(output.stdout.trim(), vars, &cache).await?;
          Ok(output.stdout.trim().to_string())
        }
      },
      Err(ExecutorError::IoError(error)) if error.kind() == io::ErrorKind::Interrupted => {
        Err(ExecutorError::TaskCancelled(self.name.clone()))
      },
      Err(error) => self.handle_execution_error(&console_target, error).await,
    };
    result.map(TaskOutcome::success)
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
  async fn set_result(&self, task_name: String, result: Arc<str>) {
    let mut deps_res = self.deps_res.lock().await;

    deps_res.insert(task_name, result);
  }

  /// Replaces dependency results when an internal node propagates a bypassed branch.
  async fn bypass_result(&self, result: HashMap<String, Arc<str>>) {
    let mut deps_res = self.deps_res.lock().await;
    *deps_res = result;
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
