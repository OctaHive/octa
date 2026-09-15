//! Resolves the filesystem contract of a cacheable task.
//!
//! User declarations and plugin plans are combined here before cache lookup.
//! Each producer retains its own ordered input rules, preventing an exclusion
//! from one plugin from hiding an input required by another producer. The
//! resulting canonical contract is then used both for snapshots and action
//! identity.

use super::*;

use octa_plugin::protocol::{PluginCachePlan, TargetPlatform};
use octa_plugin_manager::plugin_manager::PluginCachePlanningRequest;

/// Canonical filesystem contract produced by user declarations and plugins.
pub(super) struct EffectiveFileContract {
  pub(super) input_pattern_sets: Vec<Vec<String>>,
  pub(super) outputs: Vec<RelativePath>,
  /// Exact task keys whose implementations contribute execution semantics.
  pub(super) plugin_keys: Vec<String>,
}

/// One executable plugin invocation that can observe or change the workspace.
struct CachePlanInvocation {
  key: String,
  value: serde_json::Value,
}

impl TaskGraphBuilder {
  /// Obtains complete plugin contracts and unions them with explicit files.
  pub(super) async fn effective_file_contract(&self, command: &FindResult) -> ExecutorResult<EffectiveFileContract> {
    let explicit_inputs = command.task.files.as_ref().and_then(|files| files.inputs.clone());
    let mut input_pattern_sets = explicit_inputs.clone().into_iter().collect::<Vec<_>>();
    let mut outputs = command
      .task
      .files
      .as_ref()
      .map(|files| files.outputs.clone())
      .unwrap_or_default();
    let working_directory = self.cache_planning_working_directory(command)?;
    let target = TargetPlatform {
      os: self.os_type.clone(),
      architecture: self.os_arch.clone(),
    };

    let invocations = self.cache_plan_invocations(command)?;
    let mut plugin_keys = invocations
      .iter()
      .map(|invocation| invocation.key.clone())
      .collect::<Vec<_>>();
    plugin_keys.sort();
    plugin_keys.dedup();

    for invocation in invocations {
      let name = invocation.key;
      let request = PluginCachePlanningRequest {
        value: invocation.value,
        working_directory: working_directory.clone(),
        target: target.clone(),
      };
      let plan = self
        .plugin_manager
        .plan_cache_for_key(&name, request)
        .await
        .map_err(|error| {
          ExecutorError::InvalidCacheConfiguration(format!(
            "plugin step '{name}' could not plan its filesystem contract: {error}"
          ))
        })?;

      match plan {
        Some(PluginCachePlan {
          inputs: plugin_inputs,
          outputs: plugin_outputs,
        }) => {
          input_pattern_sets.push(plugin_inputs);
          outputs.extend(plugin_outputs);
        },
        None if explicit_inputs.is_none() => {
          return Err(ExecutorError::InvalidCacheConfiguration(format!(
            "task '{}' omits files.inputs but plugin step '{name}' does not provide a complete filesystem contract",
            command.name
          )));
        },
        None => {},
      }
    }

    // Pattern order inside one producer's set is semantic, while producer
    // order is not. Canonicalizing sets and roots avoids needless cache misses
    // when independent commands are reordered.
    input_pattern_sets.retain(|patterns| !patterns.is_empty());
    input_pattern_sets.sort();
    input_pattern_sets.dedup();
    outputs.sort();
    outputs.dedup();
    let outputs = outputs
      .into_iter()
      .map(RelativePath::new)
      .collect::<Result<Vec<_>, _>>()
      .map_err(|error| ExecutorError::InvalidCacheConfiguration(error.to_string()))?;
    if let Some(path) = outputs
      .iter()
      .find(|path| octa_cache_protocol::is_octa_workspace_state_path(path))
    {
      return Err(ExecutorError::InvalidCacheConfiguration(format!(
        "cache output '{}' uses Octa's reserved .octa workspace state",
        path.as_str()
      )));
    }

    Ok(EffectiveFileContract {
      input_pattern_sets,
      outputs,
      plugin_keys,
    })
  }

  /// Converts the task directory to the portable path exposed to plugins.
  ///
  /// Cache planning happens before runtime template resolution. Accepting a
  /// templated directory here could plan and hash a different filesystem than
  /// the task eventually observes, so such a cache boundary is rejected.
  fn cache_planning_working_directory(&self, command: &FindResult) -> ExecutorResult<String> {
    let working_directory = self.task_working_dir(command);
    let text = working_directory.to_string_lossy();
    if text.contains("{{") && text.contains("}}") {
      return Err(ExecutorError::InvalidCacheConfiguration(format!(
        "task '{}' cache working directory must be concrete before plugin cache planning",
        command.name
      )));
    }
    let relative = working_directory.strip_prefix(&self.dir).map_err(|_| {
      ExecutorError::InvalidCacheConfiguration(format!(
        "task '{}' working directory '{}' is outside the cache workspace",
        command.name,
        working_directory.display()
      ))
    })?;
    if relative.as_os_str().is_empty() {
      return Ok(String::new());
    }
    RelativePath::from_path(relative)
      .map(|path| path.as_str().to_owned())
      .map_err(|error| ExecutorError::InvalidCacheConfiguration(error.to_string()))
  }

  /// Lists each distinct platform-active plugin execution that can affect the result.
  ///
  /// Preconditions are template expressions rather than plugin commands. Any
  /// helper they invoke is evaluated before lookup and tracked in the runtime
  /// action identity, so it must not be invented here as a shell execution.
  fn cache_plan_invocations(&self, command: &FindResult) -> ExecutorResult<Vec<CachePlanInvocation>> {
    let mut invocations = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |key: String, value: &serde_yml::Value| -> ExecutorResult<()> {
      let value = serde_json::to_value(value)
        .map_err(|error| ExecutorError::ExtraValueConvertError(key.clone(), error.to_string()))?;
      let serialized = serde_json::to_string(&value)
        .map_err(|error| ExecutorError::ExtraValueConvertError(key.clone(), error.to_string()))?;
      if seen.insert((key.clone(), serialized)) {
        invocations.push(CachePlanInvocation { key, value });
      }
      Ok(())
    };

    if let Some(plugin) = &command.task.plugin {
      push(plugin.key.clone(), &plugin.value)?;
    }
    if let Some(conditions) = &command.task.condition {
      for condition in conditions.before_deps.iter().chain(conditions.after_deps.iter()) {
        push(condition.command.key.clone(), &condition.command.value)?;
      }
    }
    for task_command in command.task.cmds.as_deref().unwrap_or_default() {
      if !self.matches_platforms(task_command.options.platforms.as_deref()) {
        continue;
      }
      if let Some(condition) = &task_command.options.condition {
        push(condition.key.clone(), &condition.value)?;
      }
      if let CommandPayload::Plugin(plugin) = &task_command.payload {
        push(plugin.key.clone(), &plugin.value)?;
      }
    }
    Ok(invocations)
  }
}
