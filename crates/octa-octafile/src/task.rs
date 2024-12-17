use std::path::PathBuf;

use serde::{
  de::{MapAccess, Visitor},
  Deserialize, Deserializer, Serialize,
};

use crate::{octafile::Envs, Cmds, Vars};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecuteMode {
  Parallel,
  Sequentially,
}

impl From<String> for ExecuteMode {
  fn from(value: String) -> Self {
    match value.as_str() {
      "parallel" => ExecuteMode::Parallel,
      "sequentially" => ExecuteMode::Sequentially,
      _ => unimplemented!(),
    }
  }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AllowedPlatforms {
  Linux,
  Windows,
  Darwin,
}

impl From<String> for AllowedPlatforms {
  fn from(value: String) -> Self {
    match value.as_str() {
      "windows" => AllowedPlatforms::Windows,
      "darwin" => AllowedPlatforms::Darwin,
      "linux" => AllowedPlatforms::Linux,
      _ => unimplemented!(),
    }
  }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceStrategies {
  Timestamp,
  Hash,
}

impl From<String> for SourceStrategies {
  fn from(value: String) -> Self {
    match value.as_str() {
      "timestamp" => SourceStrategies::Timestamp,
      "hash" => SourceStrategies::Hash,
      _ => unimplemented!(),
    }
  }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AllowedRun {
  Always,
  Once,
  Changed,
}

impl From<String> for AllowedRun {
  fn from(value: String) -> Self {
    match value.as_str() {
      "once" => AllowedRun::Once,
      "always" => AllowedRun::Always,
      "changed" => AllowedRun::Changed,
      _ => unimplemented!(),
    }
  }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ComplexDep {
  pub task: String,
  pub vars: Option<Vars>,
  pub silent: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Deps {
  Simple(String),
  Complex(ComplexDep),
}

impl From<String> for Deps {
  fn from(context: String) -> Self {
    Self::Simple(context)
  }
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct Task {
  pub env: Option<Envs>,                         // Task environment variables
  pub dir: Option<PathBuf>,                      // Working directory for the task
  pub desc: Option<String>,                      // Task description
  pub vars: Option<Vars>,                        // Task-specific variables
  pub tpl: Option<String>,                       // Task template
  pub cmd: Option<Cmds>,                         // Command to execute
  pub cmds: Option<Vec<Cmds>>,                   // List of commands
  pub internal: Option<bool>,                    // Show command in list of available commands
  pub platforms: Option<Vec<AllowedPlatforms>>,  // Supported platforms
  pub ignore_error: Option<bool>,                // Whether to continue on error
  pub deps: Option<Vec<Deps>>,                   // Task dependencies
  pub run: Option<AllowedRun>,                   // When task should run
  pub silent: Option<bool>,                      // Should task print to stdout or stderr
  pub execute_mode: Option<ExecuteMode>,         // How execute task commands
  pub sources: Option<Vec<String>>,              // Sources for fingerprinting
  pub source_strategy: Option<SourceStrategies>, // Strategy for compare sources
  pub predonditions: Option<Vec<String>>,        // Commands to check should run command
}

impl<'de> Deserialize<'de> for Task {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct TaskVisitor;

    impl<'de> Visitor<'de> for TaskVisitor {
      type Value = Task;

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a string or a map representing a Task")
      }

      fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
      where
        E: serde::de::Error,
      {
        Ok(Task {
          cmd: Some(Cmds::Simple(value.to_string())),
          ..Task::default()
        })
      }

      fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
      where
        M: MapAccess<'de>,
      {
        let mut task = Task::default();

        while let Some(key) = map.next_key::<String>()? {
          match key.as_str() {
            "dir" => task.dir = map.next_value()?,
            "desc" => task.desc = map.next_value()?,
            "vars" => task.vars = map.next_value()?,
            "tpl" => task.tpl = map.next_value()?,
            "cmd" => task.cmd = map.next_value()?,
            "cmds" => task.cmds = map.next_value()?,
            "internal" => task.internal = map.next_value()?,
            "platforms" => task.platforms = map.next_value()?,
            "ignore_error" => task.ignore_error = map.next_value()?,
            "deps" => task.deps = map.next_value()?,
            "run" => task.run = map.next_value()?,
            "silent" => task.silent = map.next_value()?,
            "execute_mode" => task.execute_mode = map.next_value()?,
            "sources" => task.sources = map.next_value()?,
            "source_strategy" => task.source_strategy = map.next_value()?,
            "predonditions" => task.predonditions = map.next_value()?,
            _ => {
              // Skip unknown fields
              let _ = map.next_value::<serde::de::IgnoredAny>()?;
            },
          }
        }

        Ok(task)
      }
    }

    deserializer.deserialize_any(TaskVisitor)
  }
}
