//! Configuration and shared runtime state used by executable task nodes.

use super::*;

#[derive(Clone, Debug, Default)]
pub(crate) enum NodeAction {
  #[default]
  Command,
  Barrier,
  Condition,
  FreshnessCheck {
    spec: Box<FreshnessSpec>,
    state: Arc<FreshnessState>,
  },
  FreshnessCommit(Arc<FreshnessState>),
}

impl NodeAction {
  pub(super) fn is_command(&self) -> bool {
    matches!(self, Self::Command)
  }

  fn needs_working_directory(&self) -> bool {
    matches!(self, Self::Command)
  }
}

#[derive(Clone, Debug, Default)]
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

  pub(super) fn is_managed(&self) -> bool {
    self.state.is_some()
  }

  pub(super) fn shared_runtime_context(&self) -> Option<RuntimeContext> {
    self.state.as_ref().and_then(|state| state.runtime_context())
  }
}

/// Shared result of a task-level condition evaluated by a condition gate node.
#[derive(Debug, Clone)]
struct ConditionOutcome {
  passed: bool,
  runtime_context: Option<RuntimeContext>,
}

#[derive(Debug, Default)]
pub(crate) struct ConditionState(OnceLock<ConditionOutcome>);

impl ConditionState {
  fn set(&self, passed: bool, runtime_context: Option<RuntimeContext>) {
    let _ = self.0.set(ConditionOutcome {
      passed,
      runtime_context,
    });
  }

  fn passed(&self) -> Option<bool> {
    self.0.get().map(|outcome| outcome.passed)
  }

  fn runtime_context(&self) -> Option<RuntimeContext> {
    self.0.get().and_then(|outcome| outcome.runtime_context.clone())
  }
}

#[derive(Debug, Clone, Default)]
enum SharedConditionState {
  #[default]
  None,
  Publish(Arc<ConditionState>),
  Reuse(Arc<ConditionState>),
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
}

/// Conditions and shared gate state attached to one executable graph node.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConditionRuntime {
  conditions: Vec<PluginInvocation>,
  guards: Vec<Arc<ConditionState>>,
  shared_state: SharedConditionState,
}

impl ConditionRuntime {
  pub(crate) fn command(
    conditions: Vec<PluginInvocation>,
    guards: Vec<Arc<ConditionState>>,
    runtime_context: Option<Arc<ConditionState>>,
  ) -> Self {
    Self {
      conditions,
      guards,
      shared_state: runtime_context.map_or(SharedConditionState::None, SharedConditionState::Reuse),
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
      shared_state: SharedConditionState::Publish(state),
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
          self.publish(false, None);
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

  pub(super) fn publish(&self, passed: bool, runtime_context: Option<RuntimeContext>) {
    if let SharedConditionState::Publish(state) = &self.shared_state {
      state.set(passed, runtime_context);
    }
  }

  pub(super) fn shared_runtime_context(&self) -> Option<RuntimeContext> {
    match &self.shared_state {
      SharedConditionState::Reuse(state) => state.runtime_context(),
      SharedConditionState::None | SharedConditionState::Publish(_) => None,
    }
  }
}

/// Cache implementation
#[derive(Debug)]
pub struct CacheItem {
  pub(super) result: String,
  pub(super) vars: Vars,
}

impl CacheItem {
  pub fn new(result: String, vars: Vars) -> Self {
    Self { result, vars }
  }
}

pub struct TaskConfig {
  // Task identification
  pub id: String,
  pub name: String,
  pub dep_name: String,

  // Execution configuration
  pub dir: PathBuf,        // Working directory
  pub ignore_errors: bool, // Whether to continue on error
  pub silent: bool,        // Should task print to stdout or stderr
  pub failfast: bool,      // Cancel parallel work after the first failure

  // Runtime behavior
  pub run_mode: RunMode, // Run mode
  pub vars: Vars,        // Task variables
  pub envs: Envs,        // Task environments
  pub(super) standalone_freshness: Option<FreshnessConfig>,
  pub(super) condition_runtime: ConditionRuntime, // Conditions attached to this graph node
  pub(super) freshness_runtime: FreshnessRuntime, // Task-level source and output state
  pub preconditions: Option<Vec<String>>,         // Task preconditions
  pub timeout: Option<Timeout>,                   // Maximum task execution time
  pub(super) output_scope: Option<ConsoleScope>,

  // State management
  pub(super) action: NodeAction,
  pub(super) plugin: Option<PluginInvocation>,
}

impl TaskConfig {
  pub fn builder() -> TaskConfigBuilder {
    TaskConfigBuilder::default()
  }
}

#[derive(Default)]
pub struct TaskConfigBuilder {
  id: Option<String>,
  name: Option<String>,
  dep_name: Option<String>,

  pub dir: Option<PathBuf>,
  pub ignore_errors: Option<bool>,
  pub silent: Option<bool>,
  pub failfast: Option<bool>,

  pub run_mode: Option<RunMode>,
  pub vars: Option<Vars>,
  pub envs: Option<Envs>,
  pub sources: Option<Vec<String>>,
  pub output: Option<Vec<String>>,
  pub octafile_root: Option<PathBuf>,
  pub source_strategy: Option<SourceMethod>,
  source_strategy_impl: Option<SourceStrategyHandle>,
  condition_runtime: ConditionRuntime,
  freshness_runtime: FreshnessRuntime,
  pub preconditions: Option<Vec<String>>,
  pub timeout: Option<Timeout>,
  output_scope: Option<ConsoleScope>,

  action: NodeAction,
  plugin: Option<PluginInvocation>,
}

impl TaskConfigBuilder {
  pub fn name(mut self, name: impl Into<String>) -> Self {
    self.name = Some(name.into());
    self
  }

  pub fn dep_name(mut self, dep_name: impl Into<String>) -> Self {
    self.dep_name = Some(dep_name.into());
    self
  }

  pub fn id(mut self, id: impl Into<String>) -> Self {
    self.id = Some(id.into());
    self
  }

  pub fn sources(mut self, sources: Option<Vec<String>>) -> Self {
    self.sources = sources;
    self
  }

  pub fn output(mut self, output: Option<Vec<String>>) -> Self {
    self.output = output;
    self
  }

  pub fn octafile_root(mut self, octafile_root: impl Into<PathBuf>) -> Self {
    self.octafile_root = Some(octafile_root.into());
    self
  }

  pub fn preconditions(mut self, preconditions: Option<Vec<String>>) -> Self {
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

  pub fn timeout(mut self, timeout: Option<Timeout>) -> Self {
    self.timeout = timeout;
    self
  }

  pub(crate) fn output_scope(mut self, output_scope: Option<ConsoleScope>) -> Self {
    self.output_scope = output_scope;
    self
  }

  pub fn vars(mut self, vars: Vars) -> Self {
    self.vars = Some(vars);
    self
  }

  pub fn envs(mut self, envs: Envs) -> Self {
    self.envs = Some(envs);
    self
  }

  pub fn dir(mut self, dir: impl Into<PathBuf>) -> Self {
    self.dir = Some(dir.into());
    self
  }

  pub(crate) fn plugin(mut self, plugin: Option<PluginInvocation>) -> Self {
    self.plugin = plugin;
    self
  }

  pub fn ignore_errors(mut self, ignore_errors: Option<bool>) -> Self {
    self.ignore_errors = ignore_errors;
    self
  }

  pub fn silent(mut self, silent: Option<bool>) -> Self {
    self.silent = silent;
    self
  }

  pub fn failfast(mut self, failfast: Option<bool>) -> Self {
    self.failfast = failfast;
    self
  }

  pub fn run_mode(mut self, run_mode: Option<impl Into<RunMode>>) -> Self {
    self.run_mode = run_mode.map(Into::into);
    self
  }

  pub fn source_strategy(mut self, source_strategy: Option<impl Into<SourceMethod>>) -> Self {
    self.source_strategy = source_strategy.map(Into::into);
    self
  }

  /// Supplies a named fingerprint implementation used by a directly constructed task.
  pub fn source_strategy_provider<S>(mut self, method: SourceMethod, strategy: S) -> Self
  where
    S: SourceStrategy + 'static,
  {
    self.source_strategy = Some(method);
    self.source_strategy_impl = Some(SourceStrategyHandle::new(strategy));
    self
  }

  pub(crate) fn action(mut self, action: NodeAction) -> Self {
    self.action = action;
    self
  }

  pub fn build(self) -> ExecutorResult<TaskConfig> {
    let dir = match self.dir {
      Some(dir) => dir,
      None if self.action.needs_working_directory() => return Err(ExecutorError::TaskConfigFieldMissing("dir")),
      None => PathBuf::new(),
    };
    let standalone_freshness = if self.sources.is_some() || self.output.is_some() {
      let method = self.source_strategy.unwrap_or(SourceMethod::Hash);
      let strategy = match self.source_strategy_impl {
        Some(strategy) => strategy,
        None => SourceStrategyRegistry::default().resolve(&method)?,
      };
      Some(FreshnessConfig::new(
        self.sources,
        self.output,
        self.octafile_root.unwrap_or_else(|| dir.clone()),
        method,
        strategy,
      ))
    } else {
      None
    };

    Ok(TaskConfig {
      id: self.id.ok_or(ExecutorError::TaskConfigFieldMissing("id"))?,
      name: self.name.ok_or(ExecutorError::TaskConfigFieldMissing("name"))?,
      dep_name: self.dep_name.ok_or(ExecutorError::TaskConfigFieldMissing("dep_name"))?,
      dir,
      ignore_errors: self.ignore_errors.unwrap_or(false),
      silent: self.silent.unwrap_or(false),
      failfast: self.failfast.unwrap_or(false),
      run_mode: self.run_mode.unwrap_or(RunMode::Always),
      vars: self.vars.unwrap_or_default(),
      envs: self.envs.unwrap_or_default(),
      standalone_freshness,
      condition_runtime: self.condition_runtime,
      freshness_runtime: self.freshness_runtime,
      preconditions: self.preconditions,
      timeout: self.timeout,
      output_scope: self.output_scope,
      action: self.action,
      plugin: self.plugin,
    })
  }
}
