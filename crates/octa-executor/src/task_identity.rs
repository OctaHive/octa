//! Stable, executor-owned task semantics used by persistent action identities.
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
  // whether and how it participates in the persistent action identity.
  let Task {
    env,
    dotenv,
    dir,
    desc: _,
    prefix: _,
    presentation: _,
    vars,
    cmds,
    internal: _,
    platforms: _,
    ignore_error,
    deps,
    run: _,
    quiet: _,
    silent: _,
    raw,
    interactive,
    execute_mode,
    failfast: _,
    timeout,
    files,
    cache: _,
    outputs,
    artifacts,
    reports,
    watch: _,
    condition,
    preconditions,
    plugin,
  } = task;

  Ok(json!({
    "env": env,
    "dotenv": dotenv,
    "dir": dir,
    "vars": vars,
    "cmds": cmds.as_deref().map(command_definitions).transpose()?,
    "ignore_error": ignore_error,
    "deps": deps.as_deref().map(dep_definitions),
    "raw": raw,
    "interactive": interactive,
    "execute_mode": execute_mode,
    "timeout": timeout,
    "files": files,
    "outputs": outputs,
    "artifacts": artifacts,
    "reports": reports,
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
    id,
    platforms,
    deferred,
    timeout,
    condition,
    quiet: _,
    silent: _,
    raw,
    ignore_error,
  } = options;

  Ok(json!({
    "payload": payload,
    "options": {
      "platforms": platforms,
      "id": id,
      "deferred": deferred,
      "timeout": timeout,
      "condition": condition.as_ref().map(plugin_definition).transpose()?,
      "raw": raw,
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
    quiet: _,
    silent: _,
    raw,
    interactive,
    timeout,
  } = dep;
  json!({
    "task": task,
    "vars": vars,
    "envs": envs,
    "raw": raw,
    "interactive": interactive,
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
  serde_json::to_value(value).map_err(ExecutorError::ActionIdentitySerialization)
}

#[cfg(test)]
mod tests {
  use octa_octafile::{AllowedRun, ConditionEvaluation, ExecuteMode, Silence, TaskFiles};

  use super::*;

  #[test]
  fn dependency_and_condition_definitions_preserve_their_distinct_shapes() {
    let dependencies = dep_definitions(&[
      Deps::Simple("prepare".to_owned()),
      Deps::Complex(ComplexDep {
        task: "build".to_owned(),
        vars: None,
        envs: None,
        quiet: None,
        silent: None,
        raw: Some(true),
        interactive: Some(true),
        timeout: None,
      }),
    ]);
    assert_eq!(dependencies[0]["type"], "simple");
    assert_eq!(dependencies[1]["type"], "complex");
    assert_eq!(dependencies[1]["value"]["raw"], true);

    let plugin = |value: &str| PluginCommand {
      key: "shell".to_owned(),
      value: serde_yml::Value::String(value.to_owned()),
    };
    let conditions = condition_definitions(&TaskConditions {
      before_deps: Some(TaskCondition {
        command: plugin("before"),
        evaluate: ConditionEvaluation::Once,
      }),
      after_deps: Some(TaskCondition {
        command: plugin("after"),
        evaluate: ConditionEvaluation::PerCommand,
      }),
    })
    .unwrap();
    assert_eq!(conditions["before_deps"]["evaluate"], "once");
    assert_eq!(conditions["after_deps"]["evaluate"], "per_command");
  }

  #[test]
  fn semantic_definition_includes_execution_but_excludes_presentation_policy() {
    let plugin = |value: &str| PluginCommand {
      key: "shell".to_owned(),
      value: serde_yml::Value::String(value.to_owned()),
    };
    let task = Task {
      dir: Some("work".into()),
      desc: Some("human description".to_owned()),
      prefix: Some("pretty".to_owned()),
      cmds: Some(vec![TaskCommand {
        payload: CommandPayload::Plugin(plugin("build")),
        options: CommandOptions {
          id: Some("compile".to_owned()),
          platforms: Some(vec!["linux".to_owned()]),
          deferred: true,
          timeout: Some(serde_yml::from_str("1s").unwrap()),
          condition: Some(plugin("test -f input")),
          quiet: Some(true),
          silent: Some(Silence::All),
          raw: Some(false),
          ignore_error: Some(false),
        },
      }]),
      run: Some(AllowedRun::Once),
      quiet: Some(true),
      silent: Some(Silence::All),
      execute_mode: Some(ExecuteMode::Sequentially),
      files: Some(TaskFiles {
        inputs: Some(vec!["input".to_owned()]),
        outputs: vec!["output".to_owned()],
      }),
      ..Task::default()
    };

    let identity = task_definition(&task).unwrap();
    assert_eq!(identity["dir"], "work");
    assert_eq!(identity["cmds"][0]["payload"]["type"], "plugin");
    assert_eq!(identity["cmds"][0]["options"]["id"], "compile");
    assert_eq!(identity["cmds"][0]["options"]["deferred"], true);
    assert_eq!(identity["files"]["outputs"][0], "output");
    assert!(identity.get("desc").is_none());
    assert!(identity.get("prefix").is_none());
    assert!(identity.get("run").is_none());
    assert!(identity.get("quiet").is_none());
    assert!(identity.get("silent").is_none());
  }
}
