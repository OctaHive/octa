use std::{collections::HashMap, fmt, path::PathBuf, time::Duration};

use serde::{
  de::{DeserializeSeed, MapAccess, Visitor},
  Deserialize, Deserializer, Serialize,
};

use serde_yml::Value;

use crate::{octafile::Envs, Vars};

pub type PluginSchemas = HashMap<String, Option<serde_json::Map<String, serde_json::Value>>>;

/// A validated, non-zero timeout parsed from a human-readable duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(Duration);

impl Timeout {
  /// Returns the duration used by the executor.
  pub fn duration(self) -> Duration {
    self.0
  }
}

impl fmt::Display for Timeout {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    humantime::format_duration(self.0).fmt(formatter)
  }
}

impl<'de> Deserialize<'de> for Timeout {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = String::deserialize(deserializer)?;
    let duration = humantime::parse_duration(&value).map_err(serde::de::Error::custom)?;
    if duration.is_zero() {
      return Err(serde::de::Error::custom("timeout must be greater than zero"));
    }

    Ok(Self(duration))
  }
}

impl Serialize for Timeout {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    serializer.serialize_str(&self.to_string())
  }
}

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
#[serde(deny_unknown_fields)]
pub struct ComplexDep {
  pub task: String,
  pub vars: Option<Vars>,
  pub envs: Option<Envs>,
  pub silent: Option<bool>,
  pub timeout: Option<Timeout>,
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

/// A command payload with metadata handled by Octa rather than a plugin.
#[derive(Debug, Clone)]
pub struct TaskCommand {
  /// Shell command, task reference, or plugin-specific command value.
  pub value: Value,

  /// Platforms on which this command is included in the execution plan.
  pub platforms: Option<Vec<String>>,

  /// Whether the command must run after the main execution plan finishes.
  pub deferred: bool,

  /// Maximum time allowed for this command, overriding the task default.
  pub timeout: Option<Timeout>,
}

pub struct Context {
  validators: HashMap<String, Option<jsonschema::Validator>>,
}

impl Context {
  pub fn from_keys(keys: Vec<String>) -> Self {
    let mut validators = keys.into_iter().map(|key| (key, None)).collect::<HashMap<_, _>>();
    validators.entry("shell".to_string()).or_insert(None);

    Self { validators }
  }

  pub fn from_schemas(schemas: PluginSchemas) -> Result<Self, String> {
    let mut validators = HashMap::with_capacity(schemas.len());

    for (key, schema) in schemas {
      if key.is_empty() {
        return Err("plugin task key cannot be empty".to_string());
      }

      let validator = schema
        .map(|schema| {
          let schema = serde_json::Value::Object(schema);
          jsonschema::validator_for(&schema)
            .map_err(|error| format!("invalid validation schema for plugin '{key}': {error}"))
        })
        .transpose()?;
      validators.insert(key, validator);
    }

    Ok(Self { validators })
  }

  pub fn contains(&self, key: &str) -> bool {
    self.validators.contains_key(key)
  }

  pub fn validate(&self, key: &str, value: &Value) -> Result<(), String> {
    let Some(Some(validator)) = self.validators.get(key) else {
      return Ok(());
    };

    let instance = serde_json::to_value(value).map_err(|error| error.to_string())?;
    let errors = validator
      .iter_errors(&instance)
      .map(|error| error.to_string())
      .collect::<Vec<_>>();

    if errors.is_empty() {
      Ok(())
    } else {
      Err(format!("invalid parameters for plugin '{key}': {}", errors.join("; ")))
    }
  }

  fn parse_commands(&self, commands: Vec<Value>) -> Result<Vec<TaskCommand>, String> {
    commands
      .into_iter()
      .map(|command| self.parse_command(command))
      .collect()
  }

  fn parse_command(&self, command: Value) -> Result<TaskCommand, String> {
    let (value, platforms, deferred, timeout) = match command {
      Value::String(value) => (Value::String(value), None, false, None),
      Value::Mapping(mut mapping) => {
        // Command metadata belongs to the wrapper and must not be passed to a plugin schema.
        let platforms = mapping
          .remove("platforms")
          .map(serde_yml::from_value::<Vec<String>>)
          .transpose()
          .map_err(|error| format!("invalid command platforms: {error}"))?;
        let timeout = mapping
          .remove("timeout")
          .map(serde_yml::from_value::<Timeout>)
          .transpose()
          .map_err(|error| format!("invalid command timeout: {error}"))?;
        if let Some(value) = mapping.remove("defer") {
          // `defer` wraps exactly one ordinary command. Sibling command fields would make the
          // command type ambiguous, while metadata such as `platforms` and `timeout` was removed above.
          if !mapping.is_empty() {
            return Err("a deferred command cannot contain sibling command fields".to_string());
          }
          (value, platforms, true, timeout)
        } else {
          (Value::Mapping(mapping), platforms, false, timeout)
        }
      },
      _ => return Err("commands must be strings, task references, or plugin commands".to_string()),
    };

    // Validate the unwrapped value exactly like a regular command. This also delegates plugin
    // parameters to the validation schema supplied by that plugin.
    match &value {
      Value::String(_) => self.validate("shell", &value)?,
      Value::Mapping(mapping) if mapping.contains_key("task") => {
        serde_yml::from_value::<ComplexDep>(value.clone()).map_err(|error| format!("invalid task command: {error}"))?;
      },
      Value::Mapping(mapping) => {
        if mapping.len() != 1 {
          return Err("a plugin command must contain exactly one plugin task type".to_string());
        }

        let (key, value) = mapping.iter().next().unwrap();
        if !self.contains(key) {
          return Err(format!("unknown plugin command type '{key}'"));
        }
        self.validate(key, value)?;
      },
      _ => return Err("commands must be strings, task references, or plugin commands".to_string()),
    }

    Ok(TaskCommand {
      value,
      platforms,
      deferred,
      timeout,
    })
  }
}

impl Default for Context {
  fn default() -> Self {
    Self::from_keys(Vec::new())
  }
}

#[derive(Debug, Clone, Default)]
pub struct Task {
  pub env: Option<Envs>,                         // Task environment variables
  pub dotenv: Option<Vec<String>>,               // Environment files applied to this task
  pub dir: Option<PathBuf>,                      // Working directory for the task
  pub desc: Option<String>,                      // Task description
  pub vars: Option<Vars>,                        // Task-specific variables
  pub cmds: Option<Vec<TaskCommand>>,            // List of commands
  pub internal: Option<bool>,                    // Show command in list of available commands
  pub platforms: Option<Vec<String>>,            // Supported platforms
  pub ignore_error: Option<bool>,                // Whether to continue on error
  pub deps: Option<Vec<Deps>>,                   // Task dependencies
  pub run: Option<AllowedRun>,                   // When task should run
  pub silent: Option<bool>,                      // Should task print to stdout or stderr
  pub execute_mode: Option<ExecuteMode>,         // How execute task commands
  pub timeout: Option<Timeout>,                  // Default timeout for task commands
  pub sources: Option<Vec<String>>,              // Sources for fingerprinting
  pub source_strategy: Option<SourceStrategies>, // Strategy for compare sources
  pub watch: Option<bool>,                       // Watch sources and rerun the task
  pub condition: Option<String>,                 // Shell condition for task execution
  pub preconditions: Option<Vec<String>>,        // Commands to check should run command
  pub extra: HashMap<String, Value>,             // Captures any additional attributes
}

pub struct TaskSeed<'a> {
  pub context: &'a Context,
}

impl<'de> DeserializeSeed<'de> for TaskSeed<'_> {
  type Value = Task;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: Deserializer<'de>,
  {
    // Forward to a visitor, passing the context
    deserializer.deserialize_map(TaskVisitor { context: self.context })
  }
}

pub struct TaskVisitor<'a> {
  pub context: &'a Context,
}

impl<'de> Visitor<'de> for TaskVisitor<'_> {
  type Value = Task;

  fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
    formatter.write_str("a string or a map representing a Task")
  }

  fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    let mut extra = HashMap::new();
    let cmd_value = Value::String(value.to_string());
    self.context.validate("shell", &cmd_value).map_err(E::custom)?;
    extra.insert("shell".to_owned(), cmd_value);

    Ok(Task {
      extra,
      ..Task::default()
    })
  }

  fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
  where
    M: MapAccess<'de>,
  {
    let mut task = Task::default();
    let mut extra = HashMap::new();

    while let Some(key) = map.next_key::<String>()? {
      match key.as_str() {
        "dir" => task.dir = map.next_value()?,
        "desc" => task.desc = map.next_value()?,
        "vars" => task.vars = map.next_value()?,
        "env" => task.env = map.next_value()?,
        "dotenv" => task.dotenv = map.next_value()?,
        "cmds" => {
          let commands: Option<Vec<Value>> = map.next_value()?;
          task.cmds = commands
            .map(|commands| self.context.parse_commands(commands))
            .transpose()
            .map_err(serde::de::Error::custom)?;
        },
        "internal" => task.internal = map.next_value()?,
        "platforms" => task.platforms = map.next_value()?,
        "ignore_error" => task.ignore_error = map.next_value()?,
        "deps" => task.deps = map.next_value()?,
        "run" => task.run = map.next_value()?,
        "silent" => task.silent = map.next_value()?,
        "execute_mode" => task.execute_mode = map.next_value()?,
        "timeout" => task.timeout = map.next_value()?,
        "sources" => task.sources = map.next_value()?,
        "source_strategy" => task.source_strategy = map.next_value()?,
        "watch" => task.watch = map.next_value()?,
        "if" => task.condition = map.next_value()?,
        "preconditions" => task.preconditions = map.next_value()?,
        key => {
          if !self.context.contains(key) {
            return Err(serde::de::Error::custom(format!("unknown task field '{key}'")));
          }

          if !extra.is_empty() {
            return Err(serde::de::Error::custom(
              "a task cannot define more than one plugin task type",
            ));
          }

          let value = map.next_value()?;
          self.context.validate(key, &value).map_err(serde::de::Error::custom)?;
          extra.insert(key.to_owned(), value);
        },
      }
    }

    if !extra.is_empty() {
      if task.cmds.is_some() {
        return Err(serde::de::Error::custom(
          "a task cannot define both 'cmds' and a plugin task type",
        ));
      }
      task.extra = extra;
    }

    Ok(task)
  }
}
