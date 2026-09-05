//! Stable, executor-owned representations used by freshness fingerprints.
//!
//! These values deliberately do not reuse `Serialize` on the public parser
//! model. Parsing and cache identity can therefore evolve independently.

use octa_octafile::{
  CommandOptions, CommandPayload, ComplexDep, Deps, PluginCommand, Task, TaskCommand, TaskCondition, TaskConditions,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{ExecutorError, ExecutorResult};

pub(crate) fn task_definition(task: &Task) -> ExecutorResult<Value> {
  // Keep this destructuring exhaustive: adding a Task field must force an explicit decision about
  // whether and how it participates in the persisted freshness identity.
  let Task {
    env,
    dotenv,
    dir,
    desc,
    prefix,
    vars,
    cmds,
    internal,
    platforms,
    ignore_error,
    deps,
    run,
    silent,
    execute_mode,
    failfast,
    timeout,
    sources,
    output,
    source_strategy,
    watch,
    condition,
    preconditions,
    plugin,
  } = task;

  Ok(json!({
    "env": env,
    "dotenv": dotenv,
    "dir": dir,
    "desc": desc,
    "prefix": prefix,
    "vars": vars,
    "cmds": cmds.as_deref().map(command_definitions).transpose()?,
    "internal": internal,
    "platforms": platforms,
    "ignore_error": ignore_error,
    "deps": deps.as_deref().map(dep_definitions),
    "run": run,
    "silent": silent,
    "execute_mode": execute_mode,
    "failfast": failfast,
    "timeout": timeout,
    "sources": sources,
    "output": output,
    "source_strategy": source_strategy,
    "watch": watch,
    "condition": condition.as_ref().map(condition_definitions).transpose()?,
    "preconditions": preconditions,
    "plugin": plugin.as_ref().map(plugin_definition).transpose()?,
  }))
}

fn command_definitions(commands: &[TaskCommand]) -> ExecutorResult<Vec<Value>> {
  commands.iter().map(command_definition).collect()
}

fn command_definition(command: &TaskCommand) -> ExecutorResult<Value> {
  let TaskCommand { payload, options } = command;
  let payload = match payload {
    CommandPayload::Task(task) => json!({ "type": "task", "value": complex_dep_definition(task) }),
    CommandPayload::Plugin(plugin) => json!({ "type": "plugin", "value": plugin_definition(plugin)? }),
  };
  let CommandOptions {
    platforms,
    deferred,
    timeout,
    condition,
    silent,
    ignore_error,
  } = options;

  Ok(json!({
    "payload": payload,
    "options": {
      "platforms": platforms,
      "deferred": deferred,
      "timeout": timeout,
      "condition": condition.as_ref().map(plugin_definition).transpose()?,
      "silent": silent,
      "ignore_error": ignore_error,
    },
  }))
}

fn dep_definitions(deps: &[Deps]) -> Vec<Value> {
  deps
    .iter()
    .map(|dep| match dep {
      Deps::Simple(task) => json!({ "type": "simple", "task": task }),
      Deps::Complex(dep) => json!({ "type": "complex", "value": complex_dep_definition(dep) }),
    })
    .collect()
}

fn complex_dep_definition(dep: &ComplexDep) -> Value {
  let ComplexDep {
    task,
    vars,
    envs,
    silent,
    timeout,
  } = dep;
  json!({
    "task": task,
    "vars": vars,
    "envs": envs,
    "silent": silent,
    "timeout": timeout,
  })
}

fn condition_definitions(conditions: &TaskConditions) -> ExecutorResult<Value> {
  let TaskConditions {
    before_deps,
    after_deps,
  } = conditions;
  Ok(json!({
    "before_deps": before_deps.as_ref().map(condition_definition).transpose()?,
    "after_deps": after_deps.as_ref().map(condition_definition).transpose()?,
  }))
}

fn condition_definition(condition: &TaskCondition) -> ExecutorResult<Value> {
  let TaskCondition { command, evaluate } = condition;
  Ok(json!({
    "command": plugin_definition(command)?,
    "evaluate": match evaluate {
      octa_octafile::ConditionEvaluation::Once => "once",
      octa_octafile::ConditionEvaluation::PerCommand => "per_command",
    },
  }))
}

fn plugin_definition(plugin: &PluginCommand) -> ExecutorResult<Value> {
  let PluginCommand { key, value } = plugin;
  Ok(json!({
    "key": key,
    "value": json_value(value)?,
  }))
}

fn json_value<T: Serialize>(value: &T) -> ExecutorResult<Value> {
  serde_json::to_value(value).map_err(|error| ExecutorError::FreshnessIdentityError(error.to_string()))
}
