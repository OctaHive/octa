//! Configuration and shared runtime state used by executable task nodes.

use super::*;

#[derive(Clone, Debug, Default)]
/// Work performed by a graph node after dependency scheduling.
pub(crate) enum NodeAction {
  /// Invoke the configured plugin command.
  #[default]
  Command,
  /// Preserve ordering without consuming a concurrency permit.
  Barrier,
  /// Evaluate and publish a shared condition decision.
  Condition,
  /// Evaluate source/output freshness and publish the decision.
  FreshnessCheck {
    spec: Box<FreshnessSpec>,
    state: Arc<FreshnessState>,
  },
  /// Commit a successful freshness decision after command completion.
  FreshnessCommit(Arc<FreshnessState>),
}

impl NodeAction {
  pub(super) fn is_command(&self) -> bool {
    matches!(self, Self::Command)
  }

  fn needs_working_directory(&self) -> bool {
    matches!(self, Self::Command)
  }

  pub(super) fn needs_runtime_lock(&self) -> bool {
    !matches!(self, Self::Barrier)
  }
}

#[derive(Clone, Debug, Default)]
/// Access to the invocation-level freshness decision shared by command nodes.
pub(crate) struct FreshnessRuntime {
  state: Option<Arc<FreshnessState>>,
}

impl FreshnessRuntime {
  pub(crate) fn guarded(state: Option<Arc<FreshnessState>>) -> Self {
    Self { state }
  }

  pub(super) fn should_run(&self) -> ExecutorResult<bool> {
    self.state.as_ref().map_or(Ok(true), |state| state.should_run())
  }

  pub(super) fn mark_condition_skipped(&self) {
    if let Some(state) = &self.state {
      state.mark_condition_skipped();
    }
  }
}

/// Shared result of a task-level condition evaluated by a condition gate node.
#[derive(Debug, Default)]
pub(crate) struct ConditionState(OnceLock<bool>);

impl ConditionState {
  fn set(&self, passed: bool) {
    let _ = self.0.set(passed);
  }

  fn passed(&self) -> Option<bool> {
    self.0.get().copied()
  }
}

/// A normalized plugin invocation shared by task commands and conditions.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PluginInvocation {
  pub(super) key: String,
  value: Value,
}

impl PluginInvocation {
  pub(crate) fn new(key: String, value: Value) -> Self {
    Self { key, value }
  }

  #[cfg(test)]
  pub(super) fn command(&self) -> String {
    match &self.value {
      Value::String(command) => command.clone(),
      value => value.to_string(),
    }
  }

  pub(super) fn value(&self) -> Value {
    self.value.clone()
  }

  pub(crate) fn key(&self) -> &str {
    &self.key
  }
}

/// Conditions and shared gate state attached to one executable graph node.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConditionRuntime {
  conditions: Vec<PluginInvocation>,
  guards: Vec<Arc<ConditionState>>,
  publish_state: Option<Arc<ConditionState>>,
}

impl ConditionRuntime {
  pub(crate) fn command(conditions: Vec<PluginInvocation>, guards: Vec<Arc<ConditionState>>) -> Self {
    Self {
      conditions,
      guards,
      publish_state: None,
    }
  }

  pub(crate) fn gate(
    condition: PluginInvocation,
    state: Arc<ConditionState>,
    guards: Vec<Arc<ConditionState>>,
  ) -> Self {
    Self {
      conditions: vec![condition],
      guards,
      publish_state: Some(state),
    }
  }

  pub(super) fn conditions(&self) -> &[PluginInvocation] {
    &self.conditions
  }

  pub(super) fn should_run(&self, task_name: &str) -> ExecutorResult<bool> {
    for guard in &self.guards {
      match guard.passed() {
        Some(true) => {},
        Some(false) => {
          self.publish(false);
          return Ok(false);
        },
        None => {
          return Err(ExecutorError::TaskFailed(format!(
            "Task '{task_name}' reached an unevaluated condition gate"
          )));
        },
      }
    }

    Ok(true)
  }

  pub(super) fn publish(&self, passed: bool) {
    if let Some(state) = &self.publish_state {
      state.set(passed);
    }
  }
}

/// Output and variable context cached for a task configured to run once.
#[derive(Debug)]
pub(crate) struct CacheItem {
  pub(super) result: String,
  pub(super) vars: Vars,
}

impl CacheItem {
  pub(crate) fn new(result: String, vars: Vars) -> Self {
    Self { result, vars }
  }
}

/// Immutable configuration consumed by one executable DAG node.
pub(crate) struct TaskConfig {
  // Task identification
  pub id: String,
  pub name: String,
  pub dep_name: String,

  // Execution configuration
  pub dir: PathBuf,        // Working directory
  pub ignore_errors: bool, // Whether to continue on error
  pub silence: octa_octafile::Silence,
  pub quiet: bool,
  pub raw: bool,
  pub interactive_session: Option<String>,
  pub failfast: bool, // Cancel parallel work after the first failure

  // Runtime behavior
  pub run_mode: RunMode, // Run mode
  pub vars: Vars,        // Task variables
  pub envs: Envs,        // Task environments
  pub(super) invocation_runtime: Option<Arc<InvocationRuntime>>,
  pub(super) condition_runtime: ConditionRuntime, // Conditions attached to this graph node
  pub(super) freshness_runtime: FreshnessRuntime, // Task-level source and output state
  pub preconditions: Option<Vec<String>>,         // Task preconditions
  pub timeout: Option<Timeout>,                   // Maximum task execution time
  pub(super) execution_binding: Option<ExecutionBinding>,
  pub(super) prefix_template: Option<String>,

  // State management
  pub(super) action: NodeAction,
  pub(super) plugin: Option<PluginInvocation>,
}

impl TaskConfig {
  pub(crate) fn builder() -> TaskConfigBuilder {
    TaskConfigBuilder::default()
  }
}

#[derive(Default)]
pub(crate) struct TaskConfigBuilder {
  id: Option<String>,
  name: Option<String>,
  dep_name: Option<String>,

  pub dir: Option<PathBuf>,
  pub ignore_errors: Option<bool>,
  pub silent: Option<octa_octafile::Silence>,
  pub quiet: Option<bool>,
  pub raw: Option<bool>,
  pub failfast: Option<bool>,

  pub run_mode: Option<RunMode>,
  pub vars: Option<Vars>,
  pub envs: Option<Envs>,
  invocation_runtime: Option<Arc<InvocationRuntime>>,
  condition_runtime: ConditionRuntime,
  freshness_runtime: FreshnessRuntime,
  pub preconditions: Option<Vec<String>>,
  pub timeout: Option<Timeout>,
  execution_binding: Option<ExecutionBinding>,
  prefix_template: Option<String>,
  interactive_session: Option<String>,

  action: NodeAction,
  plugin: Option<PluginInvocation>,
}

impl TaskConfigBuilder {
  pub(crate) fn name(mut self, name: impl Into<String>) -> Self {
    self.name = Some(name.into());
    self
  }

  pub(crate) fn dep_name(mut self, dep_name: impl Into<String>) -> Self {
    self.dep_name = Some(dep_name.into());
    self
  }

  pub(crate) fn id(mut self, id: impl Into<String>) -> Self {
    self.id = Some(id.into());
    self
  }

  pub(crate) fn preconditions(mut self, preconditions: Option<Vec<String>>) -> Self {
    self.preconditions = preconditions;
    self
  }

  pub(crate) fn condition_runtime(mut self, condition_runtime: ConditionRuntime) -> Self {
    self.condition_runtime = condition_runtime;
    self
  }

  pub(crate) fn freshness_runtime(mut self, freshness_runtime: FreshnessRuntime) -> Self {
    self.freshness_runtime = freshness_runtime;
    self
  }

  pub(crate) fn timeout(mut self, timeout: Option<Timeout>) -> Self {
    self.timeout = timeout;
    self
  }

  pub(crate) fn execution_binding(mut self, execution_binding: Option<ExecutionBinding>) -> Self {
    self.execution_binding = execution_binding;
    self
  }

  pub(crate) fn prefix_template(mut self, prefix_template: Option<String>) -> Self {
    self.prefix_template = prefix_template;
    self
  }

  pub(crate) fn interactive_session(mut self, interactive_session: Option<String>) -> Self {
    self.interactive_session = interactive_session;
    self
  }

  pub(crate) fn vars(mut self, vars: Vars) -> Self {
    self.vars = Some(vars);
    self
  }

  pub(crate) fn invocation_runtime(mut self, runtime: Option<Arc<InvocationRuntime>>) -> Self {
    self.invocation_runtime = runtime;
    self
  }

  pub(crate) fn envs(mut self, envs: Envs) -> Self {
    self.envs = Some(envs);
    self
  }

  pub(crate) fn dir(mut self, dir: impl Into<PathBuf>) -> Self {
    self.dir = Some(dir.into());
    self
  }

  pub(crate) fn plugin(mut self, plugin: Option<PluginInvocation>) -> Self {
    self.plugin = plugin;
    self
  }

  pub(crate) fn ignore_errors(mut self, ignore_errors: Option<bool>) -> Self {
    self.ignore_errors = ignore_errors;
    self
  }

  pub(crate) fn silent<T: Into<octa_octafile::Silence>>(mut self, silent: Option<T>) -> Self {
    self.silent = silent.map(Into::into);
    self
  }

  pub(crate) fn quiet(mut self, quiet: Option<bool>) -> Self {
    self.quiet = quiet;
    self
  }

  pub(crate) fn raw(mut self, raw: Option<bool>) -> Self {
    self.raw = raw;
    self
  }

  pub(crate) fn failfast(mut self, failfast: Option<bool>) -> Self {
    self.failfast = failfast;
    self
  }

  pub(crate) fn run_mode(mut self, run_mode: Option<impl Into<RunMode>>) -> Self {
    self.run_mode = run_mode.map(Into::into);
    self
  }

  pub(crate) fn action(mut self, action: NodeAction) -> Self {
    self.action = action;
    self
  }

  pub(crate) fn build(self) -> ExecutorResult<TaskConfig> {
    let dir = match self.dir {
      Some(dir) => dir,
      None if self.action.needs_working_directory() => return Err(ExecutorError::TaskConfigFieldMissing("dir")),
      None => PathBuf::new(),
    };
    Ok(TaskConfig {
      id: self.id.ok_or(ExecutorError::TaskConfigFieldMissing("id"))?,
      name: self.name.ok_or(ExecutorError::TaskConfigFieldMissing("name"))?,
      dep_name: self.dep_name.ok_or(ExecutorError::TaskConfigFieldMissing("dep_name"))?,
      dir,
      ignore_errors: self.ignore_errors.unwrap_or(false),
      silence: self.silent.unwrap_or_default(),
      quiet: self.quiet.unwrap_or(false),
      raw: self.raw.unwrap_or(false),
      interactive_session: self.interactive_session,
      failfast: self.failfast.unwrap_or(false),
      run_mode: self.run_mode.unwrap_or(RunMode::Always),
      vars: self.vars.unwrap_or_default(),
      envs: self.envs.unwrap_or_default(),
      invocation_runtime: self.invocation_runtime,
      condition_runtime: self.condition_runtime,
      freshness_runtime: self.freshness_runtime,
      preconditions: self.preconditions,
      timeout: self.timeout,
      execution_binding: self.execution_binding,
      prefix_template: self.prefix_template,
      action: self.action,
      plugin: self.plugin,
    })
  }
}
