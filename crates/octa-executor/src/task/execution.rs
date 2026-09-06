//! Runtime execution pipeline for a configured task node.

use super::*;

struct GraphActionRuntime<'a> {
  dry: bool,
  force: bool,
  cancel_token: &'a CancellationToken,
  deferred_exit_code: Option<i32>,
}

impl TaskNode {
  fn deferred_variable_overrides(exit_code: Option<i32>) -> IndexMap<String, Value> {
    exit_code
      .map(|exit_code| ("EXIT_CODE".to_owned(), Value::from(exit_code)))
      .into_iter()
      .collect()
  }

  fn expose_deferred_exit_code(vars: &mut Vars, exit_code: Option<i32>) {
    if let Some(exit_code) = exit_code {
      vars.insert("EXIT_CODE", &exit_code);
    }
  }

  pub fn new(config: TaskConfig) -> Self {
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
      standalone_freshness: config.standalone_freshness,
      vars: config.vars,
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
      output_scope: config.output_scope,
      prefix_template: config.prefix_template,
      plugin: config.plugin,
    }
  }

  #[cfg(test)]
  pub(crate) fn conditions(&self) -> Vec<String> {
    self
      .condition_runtime
      .conditions()
      .iter()
      .map(PluginInvocation::command)
      .collect()
  }

  #[cfg(test)]
  pub(crate) fn output_scope(&self) -> Option<&ConsoleScope> {
    self.output_scope.as_ref()
  }

  #[cfg(test)]
  pub(crate) fn interactive_session(&self) -> Option<&str> {
    self.interactive_session.as_deref()
  }

  pub fn watch_target(&self) -> Option<WatchTarget> {
    match &self.action {
      NodeAction::FreshnessCheck { spec, .. } => spec.watch_target(),
      _ => None,
    }
  }

  #[cfg(test)]
  pub(crate) fn source_method(&self) -> Option<SourceMethod> {
    match &self.action {
      NodeAction::FreshnessCheck { spec, .. } => Some(spec.method()),
      _ => None,
    }
  }

  #[cfg(test)]
  pub(crate) fn source_strategy_key(&self) -> Option<&'static str> {
    match &self.action {
      NodeAction::FreshnessCheck { spec, .. } => Some(spec.strategy_key()),
      _ => None,
    }
  }

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

      Ok(PathBuf::from(rendered.trim_matches('"'))) // Remove extra quotes from result
    }
  }

  #[cfg(test)]
  pub(super) async fn prepare_dir_with_vars(&self, vars: &Vars, dry: bool) -> ExecutorResult<PathBuf> {
    let dir = self
      .interpolate_dir(self.dir.clone(), vars, None, dry, CancellationToken::new())
      .await?;

    self.ensure_dir(dir, dry).await
  }

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

    let mut context = context.clone();
    Self::expose_deferred_exit_code(&mut context.vars, deferred_exit_code);
    Ok(context)
  }

  async fn log_info(&self, output: &ConsoleTarget, message: String) -> ExecutorResult<()> {
    if self.action.is_command() && !self.quiet {
      output.message(ConsoleLevel::Info, message).await?;
    }
    Ok(())
  }

  /// Executes graph-only actions before entering the plugin command path.
  async fn execute_graph_action(
    &self,
    fingerprint: &Db,
    evaluator: Arc<dyn PluginEvaluator>,
    output: &ConsoleTarget,
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
        if !runtime.dry {
          state.commit(fingerprint)?;
        }
        Ok(Some(TaskOutcome::success(String::new())))
      },
    }
  }

  pub(super) async fn standalone_freshness(
    &self,
    fingerprint: &Db,
    force: bool,
    vars: &Vars,
    envs: &Envs,
    cancel_token: &CancellationToken,
    output: &ConsoleTarget,
  ) -> ExecutorResult<Option<FreshnessOutcome>> {
    if !self.action.is_command() || self.freshness_runtime.is_managed() {
      return Ok(None);
    }
    let Some(freshness) = &self.standalone_freshness else {
      return Ok(None);
    };

    let definition = serde_json::json!({
      "dir": self.dir,
      "run_mode": self.run_mode,
      "preconditions": self.preconditions,
      "timeout": self.timeout,
      "plugin": self.plugin,
    });
    let identity = FreshnessIdentity::new(self.name.clone(), self.dep_name.clone(), definition);
    let spec = freshness.spec(identity);
    let outcome = spec.evaluate(fingerprint, force, vars, envs, cancel_token).await?;
    if !outcome.should_run() {
      self
        .log_info(output, format!("Task {} is up to date", self.dep_name))
        .await?;
    }
    Ok(Some(outcome))
  }

  fn commit_standalone_freshness(
    outcome: Option<&FreshnessOutcome>,
    fingerprint: &Db,
    dry: bool,
  ) -> ExecutorResult<()> {
    if !dry {
      if let Some(outcome) = outcome {
        outcome.commit(fingerprint)?;
      }
    }
    Ok(())
  }

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

    let deps_res = self.deps_res.lock().await;
    let mut vars = vars.clone();
    vars.insert("deps_result", &*deps_res);
    drop(deps_res);

    let invoker = PluginInvoker::new(plugin_manager);
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
    // Add dependency results to template context
    let deps_res = self.deps_res.lock().await;
    context.insert("deps_result", &*deps_res);
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

      result = result && (rendered.trim() == "true" || rendered.trim() == "True" || rendered.trim() == "1");
    }

    Ok(result)
  }

  /// Check if result is cached
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

  /// Update cache with new result
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

  async fn debug_log_dependencies(&self) {
    if enabled!(Level::DEBUG) {
      let deps = self.deps_res.lock().await;
      for (name, res) in &*deps {
        debug!("Dependency {} results: {}", name, res);
      }
    }
  }

  /// Executes the task without applying its timeout wrapper.
  async fn execute_inner(&self, runtime: TaskRuntime, cancel_token: CancellationToken) -> ExecutorResult<TaskOutcome> {
    let TaskRuntime {
      plugin_manager,
      cache,
      fingerprint,
      console,
      run_id,
      dry,
      force,
      deferred_exit_code,
    } = runtime;
    let console_target = ConsoleTarget::with_silence(console, run_id, self.output_scope.clone(), self.silence);
    let evaluator: Arc<dyn PluginEvaluator> = Arc::new(ManagerPluginEvaluator::new(plugin_manager.clone()));

    if !self.condition_runtime.should_run(&self.name)? {
      if let NodeAction::FreshnessCheck { state, .. } = &self.action {
        state.publish_skipped()?;
      }
      return Ok(TaskOutcome::skipped(String::new()));
    }

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

    let RuntimeContext { vars, envs, dir } = self
      .resolve_runtime_context(evaluator.clone(), dry, cancel_token.clone(), deferred_exit_code)
      .await?;
    if let Some(scope) = &self.output_scope {
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
      self.freshness_runtime.mark_condition_skipped();
      self
        .log_info(
          &console_target,
          format!("Task '{}' skipped because its condition was not met", self.name),
        )
        .await?;
      return Ok(TaskOutcome::skipped(String::new()));
    }

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

    let standalone_freshness = self
      .standalone_freshness(&fingerprint, force, &vars, &envs, &cancel_token, &console_target)
      .await?;
    if standalone_freshness
      .as_ref()
      .is_some_and(|outcome| !outcome.should_run())
    {
      return Ok(TaskOutcome::skipped(String::new()));
    }

    if let Some(cached) = self.check_cache(&vars, &cache).await? {
      Self::commit_standalone_freshness(standalone_freshness.as_ref(), &fingerprint, dry)?;
      return Ok(TaskOutcome::skipped(cached));
    }

    self
      .log_info(&console_target, format!("Starting task {}", self.name))
      .await?;
    self.debug_log_dependencies().await;

    let Some(plugin) = &self.plugin else {
      Self::commit_standalone_freshness(standalone_freshness.as_ref(), &fingerprint, dry)?;
      return Ok(TaskOutcome::success(String::new()));
    };
    let mut vars_with_deps_results = vars.clone();
    let deps_res = self.deps_res.lock().await;
    vars_with_deps_results.insert("deps_result", &*deps_res);

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
    let result = match PluginInvoker::new(plugin_manager)
      .invoke(request, cancel_token.clone())
      .await
    {
      Ok(output) => {
        if output.code != 0 && !cancel_token.is_cancelled() {
          if self.ignore_errors {
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
            })
          }
        } else {
          self.update_cache(output.stdout.trim(), vars, &cache).await?;
          Ok(output.stdout.trim().to_string())
        }
      },
      Err(ExecutorError::IoError(error)) if error.kind() == io::ErrorKind::Interrupted => {
        Err(ExecutorError::TaskCancelled(self.name.clone()))
      },
      Err(error) => self.handle_execution_error(&console_target, error).await,
    };
    if result.is_ok() {
      Self::commit_standalone_freshness(standalone_freshness.as_ref(), &fingerprint, dry)?;
    }
    result.map(TaskOutcome::success)
  }

  /// Handle execution errors
  async fn handle_execution_error(&self, output: &ConsoleTarget, error: ExecutorError) -> ExecutorResult<String> {
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
impl Executable<TaskNode> for TaskNode {
  /// Stores the result of a dependent task
  async fn set_result(&self, task_name: String, res: String) {
    let mut deps_res = self.deps_res.lock().await;

    deps_res.insert(task_name, res);
  }

  async fn bypass_result(&self, result: HashMap<String, String>) {
    let mut deps_res = self.deps_res.lock().await;
    *deps_res = result
  }

  /// Executes the task and returns the result
  async fn execute(&self, runtime: TaskRuntime, cancel_token: CancellationToken) -> ExecutorResult<TaskOutcome> {
    let Some(timeout) = self.timeout else {
      return self.execute_inner(runtime, cancel_token).await;
    };

    // A child token limits cancellation to this command while preserving the caller's token.
    let command_token = cancel_token.child_token();
    let output = ConsoleTarget::with_silence(
      runtime.console.clone(),
      runtime.run_id,
      self.output_scope.clone(),
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
