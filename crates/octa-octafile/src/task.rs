use std::{
  collections::{HashMap, HashSet},
  fmt,
  path::PathBuf,
  sync::Arc,
  time::Duration,
};

use indexmap::IndexMap;
use serde::{
  de::{DeserializeSeed, MapAccess, Visitor},
  Deserialize, Deserializer, Serialize,
};

use serde_yml::Value;

use crate::{octafile::Envs, TaskPresentation, Vars};

/// Static schemas exposed by one plugin task type.
#[derive(Clone, Debug, Default)]
pub struct PluginTypeSchema {
  /// Schema used while parsing plugin command values.
  pub input: Option<serde_json::Map<String, serde_json::Value>>,
  /// Schema used to validate and statically address plugin completion fields.
  pub output: Option<serde_json::Map<String, serde_json::Value>>,
}

pub type PluginSchemas = HashMap<String, PluginTypeSchema>;

// Plugin task types share YAML namespaces with task fields, command metadata, and condition
// controls. Reject collisions during plugin registration instead of parsing the same key
// differently depending on where it is used.
const RESERVED_PLUGIN_KEYS: &[&str] = &[
  "dir",
  "desc",
  "prefix",
  "presentation",
  "vars",
  "env",
  "dotenv",
  "cmds",
  "internal",
  "platforms",
  "id",
  "ignore_error",
  "deps",
  "run",
  "quiet",
  "raw",
  "interactive",
  "silent",
  "execute_mode",
  "failfast",
  "timeout",
  "sources",
  "output",
  "outputs",
  "artifacts",
  "reports",
  "source_strategy",
  "watch",
  "if",
  "preconditions",
  "task",
  "defer",
  "evaluate",
  "before_deps",
  "after_deps",
];

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

/// File or directory made available to an external runner after a task.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct ArtifactDeclaration {
  pub name: String,
  pub path: PathBuf,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub content_type: Option<String>,
}

/// Report file made available to an external runner after a task.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct ReportDeclaration {
  pub name: String,
  pub path: PathBuf,
  /// Stable format identifier supplied by the report producer.
  pub format: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStrategies {
  Timestamp,
  Hash,
  Custom(String),
}

impl From<String> for SourceStrategies {
  fn from(value: String) -> Self {
    match value.as_str() {
      "timestamp" => SourceStrategies::Timestamp,
      "hash" => SourceStrategies::Hash,
      _ => SourceStrategies::Custom(value),
    }
  }
}

impl SourceStrategies {
  pub fn as_str(&self) -> &str {
    match self {
      Self::Timestamp => "timestamp",
      Self::Hash => "hash",
      Self::Custom(value) => value,
    }
  }
}

impl Serialize for SourceStrategies {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    serializer.serialize_str(self.as_str())
  }
}

impl<'de> Deserialize<'de> for SourceStrategies {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = String::deserialize(deserializer)?;
    if value.trim().is_empty() {
      return Err(serde::de::Error::custom("source strategy must not be empty"));
    }
    Ok(value.into())
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
  pub quiet: Option<bool>,
  pub silent: Option<Silence>,
  pub raw: Option<bool>,
  pub interactive: Option<bool>,
  pub timeout: Option<Timeout>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Deps {
  Simple(String),
  Complex(ComplexDep),
}

/// Selects which command streams are hidden while retaining their captured
/// values for dependency interpolation and error reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Silence {
  #[default]
  None,
  All,
  Stdout,
  Stderr,
}

impl From<bool> for Silence {
  fn from(value: bool) -> Self {
    if value {
      Self::All
    } else {
      Self::None
    }
  }
}

impl std::str::FromStr for Silence {
  type Err = String;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    match value {
      "true" | "1" => Ok(Self::All),
      "false" | "0" => Ok(Self::None),
      "stdout" => Ok(Self::Stdout),
      "stderr" => Ok(Self::Stderr),
      _ => Err("expected true, false, stdout, or stderr".to_owned()),
    }
  }
}

impl Silence {
  pub fn hides_stdout(self) -> bool {
    matches!(self, Self::All | Self::Stdout)
  }

  pub fn hides_stderr(self) -> bool {
    matches!(self, Self::All | Self::Stderr)
  }
}

impl Serialize for Silence {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    match self {
      Self::None => serializer.serialize_bool(false),
      Self::All => serializer.serialize_bool(true),
      Self::Stdout => serializer.serialize_str("stdout"),
      Self::Stderr => serializer.serialize_str("stderr"),
    }
  }
}

impl<'de> Deserialize<'de> for Silence {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct SilenceVisitor;

    impl<'de> Visitor<'de> for SilenceVisitor {
      type Value = Silence;

      fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a boolean, 'stdout', or 'stderr'")
      }

      fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(if value { Silence::All } else { Silence::None })
      }

      fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
      where
        E: serde::de::Error,
      {
        match value {
          "stdout" => Ok(Silence::Stdout),
          "stderr" => Ok(Silence::Stderr),
          _ => Err(E::unknown_variant(value, &["stdout", "stderr"])),
        }
      }
    }

    deserializer.deserialize_any(SilenceVisitor)
  }
}

impl From<String> for Deps {
  fn from(context: String) -> Self {
    Self::Simple(context)
  }
}

/// The executable part of a task command after the Octafile has been parsed and validated.
#[derive(Debug, Clone)]
pub enum CommandPayload {
  /// A reference to another task, optionally with execution overrides.
  Task(ComplexDep),

  /// A command handled by a registered plugin.
  Plugin(PluginCommand),
}

/// Execution options shared by every command payload.
#[derive(Debug, Clone, Default)]
pub struct CommandOptions {
  /// Stable name used by task output declarations to select this step.
  pub id: Option<String>,

  /// Platforms on which this command is included in the execution plan.
  pub platforms: Option<Vec<String>>,

  /// Whether the command must run after the main execution plan finishes.
  pub deferred: bool,

  /// Maximum time allowed for this command, overriding the task default.
  pub timeout: Option<Timeout>,

  /// Plugin command that must succeed before this command runs.
  pub condition: Option<PluginCommand>,

  /// Whether output from this command is suppressed.
  pub quiet: Option<bool>,

  /// Whether one or both command output streams are suppressed.
  pub silent: Option<Silence>,

  /// Whether the command should use the exclusive raw/PTY transport.
  pub raw: Option<bool>,

  /// Whether a failure from this command is ignored.
  pub ignore_error: Option<bool>,
}

/// Controls how often a phased task condition is evaluated.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConditionEvaluation {
  /// Evaluate the condition once and share its result with the task scope.
  #[default]
  Once,

  /// Re-evaluate the condition before every command in `cmds`.
  PerCommand,
}

/// A validated plugin task type and its YAML payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
  pub key: String,
  pub value: Value,
}

/// A normalized task condition together with its evaluation policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCondition {
  pub command: PluginCommand,
  pub evaluate: ConditionEvaluation,
}

/// Task conditions normalized around dependency execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskConditions {
  pub before_deps: Option<TaskCondition>,
  pub after_deps: Option<TaskCondition>,
}

/// A typed command payload and the Octa-specific options that control its execution.
#[derive(Debug, Clone)]
pub struct TaskCommand {
  pub payload: CommandPayload,
  pub options: CommandOptions,
}

/// Selects one field returned by a named plugin command.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutput {
  /// Stable `id` of the plugin command that produces the value.
  pub step: String,
  /// Top-level field selected from that command's structured outputs.
  pub field: String,
  /// Prevents this value from being serialized and propagates redaction to consumers.
  #[serde(default)]
  pub secret: bool,
}

pub(crate) struct Context {
  plugins: Arc<HashMap<String, PluginType>>,
  default_plugin: String,
}

struct PluginType {
  input_validator: Option<jsonschema::Validator>,
  required_output_fields: HashSet<String>,
}

impl Context {
  pub(crate) fn from_keys(keys: Vec<String>, default_plugin: impl Into<String>) -> Result<Self, String> {
    let mut plugins = HashMap::with_capacity(keys.len());
    for key in keys {
      validate_plugin_key(&key)?;
      if plugins
        .insert(
          key.clone(),
          PluginType {
            input_validator: None,
            required_output_fields: HashSet::new(),
          },
        )
        .is_some()
      {
        return Err(format!("duplicated plugin task key '{key}'"));
      }
    }

    Self::new(plugins, default_plugin)
  }

  pub(crate) fn from_schemas(schemas: PluginSchemas, default_plugin: impl Into<String>) -> Result<Self, String> {
    let mut plugins = HashMap::with_capacity(schemas.len());

    for (key, schema) in schemas {
      validate_plugin_key(&key)?;
      let input_validator = schema
        .input
        .map(|schema| {
          let schema = serde_json::Value::Object(schema);
          jsonschema::validator_for(&schema)
            .map_err(|error| format!("invalid input schema for plugin '{key}': {error}"))
        })
        .transpose()?;
      let required_output_fields = required_output_fields(&key, schema.output)?;
      plugins.insert(
        key,
        PluginType {
          input_validator,
          required_output_fields,
        },
      );
    }

    Self::new(plugins, default_plugin)
  }

  fn new(plugins: HashMap<String, PluginType>, default_plugin: impl Into<String>) -> Result<Self, String> {
    let default_plugin = default_plugin.into();
    if !plugins.contains_key(&default_plugin) {
      return Err(format!("unknown default plugin '{default_plugin}'"));
    }

    Ok(Self {
      plugins: Arc::new(plugins),
      default_plugin,
    })
  }

  pub(crate) fn contains(&self, key: &str) -> bool {
    self.plugins.contains_key(key)
  }

  pub(crate) fn validate(&self, key: &str, value: &Value) -> Result<(), String> {
    let Some(validator) = self.plugins.get(key).and_then(|plugin| plugin.input_validator.as_ref()) else {
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

  pub(crate) fn with_default_plugin(&self, default_plugin: Option<String>) -> Result<Self, String> {
    let default_plugin = default_plugin.unwrap_or_else(|| self.default_plugin.clone());
    if !self.contains(&default_plugin) {
      return Err(format!("unknown default plugin '{default_plugin}'"));
    }

    Ok(Self {
      plugins: Arc::clone(&self.plugins),
      default_plugin,
    })
  }

  pub(crate) fn default_plugin(&self) -> &str {
    &self.default_plugin
  }

  fn parse_condition_command(&self, value: Value) -> Result<PluginCommand, String> {
    let (key, value) = match value {
      Value::String(command) => (self.default_plugin().to_owned(), Value::String(command)),
      Value::Mapping(mapping) if mapping.len() == 1 => mapping.into_iter().next().unwrap(),
      _ => return Err("a condition must be a string or contain exactly one plugin task type".to_string()),
    };

    if !self.contains(&key) {
      return Err(format!("unknown plugin condition type '{key}'"));
    }
    self.validate(&key, &value)?;

    Ok(PluginCommand { key, value })
  }

  fn parse_phase_condition(&self, value: Value, before_deps: bool) -> Result<TaskCondition, String> {
    let (command, evaluate) = match value {
      Value::Mapping(mut mapping) => {
        let evaluate = mapping
          .remove("evaluate")
          .map(serde_yml::from_value::<ConditionEvaluation>)
          .transpose()
          .map_err(|error| format!("invalid condition evaluation policy: {error}"))?
          .unwrap_or_default();
        (self.parse_condition_command(Value::Mapping(mapping))?, evaluate)
      },
      value => (self.parse_condition_command(value)?, ConditionEvaluation::Once),
    };

    if before_deps && evaluate == ConditionEvaluation::PerCommand {
      return Err("'before_deps' conditions only support 'evaluate: once'".to_string());
    }

    Ok(TaskCondition { command, evaluate })
  }

  fn parse_task_conditions(&self, value: Value) -> Result<TaskConditions, String> {
    if matches!(value, Value::String(_)) {
      return Ok(TaskConditions {
        before_deps: None,
        after_deps: Some(self.parse_phase_condition(value, false)?),
      });
    }

    let Value::Mapping(mut mapping) = value else {
      return Err("task condition must be a string or a mapping".to_string());
    };

    if !mapping.contains_key("before_deps") && !mapping.contains_key("after_deps") {
      return Ok(TaskConditions {
        before_deps: None,
        after_deps: Some(self.parse_phase_condition(Value::Mapping(mapping), false)?),
      });
    }

    let before_deps = mapping
      .remove("before_deps")
      .map(|value| self.parse_phase_condition(value, true))
      .transpose()?;
    let after_deps = mapping
      .remove("after_deps")
      .map(|value| self.parse_phase_condition(value, false))
      .transpose()?;

    if let Some((key, _)) = mapping.into_iter().next() {
      return Err(format!("unknown task condition field '{key}'"));
    }
    if before_deps.is_none() && after_deps.is_none() {
      return Err("task condition must define 'before_deps' or 'after_deps'".to_string());
    }

    Ok(TaskConditions {
      before_deps,
      after_deps,
    })
  }

  fn parse_commands(&self, commands: Vec<Value>) -> Result<Vec<TaskCommand>, String> {
    commands
      .into_iter()
      .map(|command| self.parse_command(command))
      .collect()
  }

  fn parse_command(&self, command: Value) -> Result<TaskCommand, String> {
    let (value, options) = match command {
      Value::String(value) => (Value::String(value), CommandOptions::default()),
      Value::Mapping(mut mapping) => {
        // Command metadata belongs to the wrapper and must not be passed to a plugin schema.
        let id = mapping
          .remove("id")
          .map(serde_yml::from_value::<String>)
          .transpose()
          .map_err(|error| format!("invalid command id: {error}"))?;
        if id.as_ref().is_some_and(|id| id.trim().is_empty()) {
          return Err("command id must not be empty".to_owned());
        }
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
        let condition = mapping
          .remove("if")
          .map(|value| self.parse_condition_command(value))
          .transpose()?;
        let quiet = mapping
          .remove("quiet")
          .map(serde_yml::from_value::<bool>)
          .transpose()
          .map_err(|error| format!("invalid command quiet option: {error}"))?;
        let silent = mapping
          .remove("silent")
          .map(serde_yml::from_value::<Silence>)
          .transpose()
          .map_err(|error| format!("invalid command silent option: {error}"))?;
        let raw = mapping
          .remove("raw")
          .map(serde_yml::from_value::<bool>)
          .transpose()
          .map_err(|error| format!("invalid command raw option: {error}"))?;
        let ignore_error = mapping
          .remove("ignore_error")
          .map(serde_yml::from_value::<bool>)
          .transpose()
          .map_err(|error| format!("invalid command ignore_error option: {error}"))?;
        let options = CommandOptions {
          id,
          platforms,
          timeout,
          condition,
          quiet,
          silent,
          raw,
          ignore_error,
          ..CommandOptions::default()
        };
        if let Some(value) = mapping.remove("defer") {
          // `defer` wraps exactly one ordinary command. Sibling command fields would make the
          // command type ambiguous, while command metadata was removed above.
          if !mapping.is_empty() {
            return Err("a deferred command cannot contain sibling command fields".to_string());
          }
          (
            value,
            CommandOptions {
              deferred: true,
              ..options
            },
          )
        } else {
          (Value::Mapping(mapping), options)
        }
      },
      _ => return Err("commands must be strings, task references, or plugin commands".to_string()),
    };

    // Validate the unwrapped value exactly like a regular command. This also delegates plugin
    // parameters to the validation schema supplied by that plugin.
    let payload = match value {
      Value::String(command) => {
        let value = Value::String(command);
        let key = self.default_plugin().to_owned();
        self.validate(&key, &value)?;
        CommandPayload::Plugin(PluginCommand { key, value })
      },
      Value::Mapping(mapping) if mapping.contains_key("task") => {
        let task = serde_yml::from_value::<ComplexDep>(Value::Mapping(mapping))
          .map_err(|error| format!("invalid task command: {error}"))?;
        CommandPayload::Task(task)
      },
      Value::Mapping(mapping) => {
        if mapping.len() != 1 {
          return Err("a plugin command must contain exactly one plugin task type".to_string());
        }

        let (key, value) = mapping.into_iter().next().unwrap();
        if !self.contains(&key) {
          return Err(format!("unknown plugin command type '{key}'"));
        }
        self.validate(&key, &value)?;
        CommandPayload::Plugin(PluginCommand { key, value })
      },
      _ => return Err("commands must be strings, task references, or plugin commands".to_string()),
    };

    Ok(TaskCommand { payload, options })
  }

  fn validate_task_outputs(&self, task: &Task) -> Result<(), String> {
    if task.outputs.as_ref().is_some_and(|outputs| !outputs.is_empty())
      && (task.sources.is_some() || task.output.is_some())
    {
      return Err("structured task outputs cannot be combined with freshness sources or file outputs".to_owned());
    }
    let mut steps = HashMap::<&str, Vec<(&str, bool)>>::new();
    for command in task.cmds.as_deref().unwrap_or_default() {
      let Some(id) = command.options.id.as_deref() else {
        continue;
      };
      let CommandPayload::Plugin(plugin) = &command.payload else {
        return Err(format!(
          "command id '{id}' belongs to a task reference, not a plugin step"
        ));
      };
      steps
        .entry(id)
        .or_default()
        .push((plugin.key.as_str(), command.options.deferred));
    }

    let Some(outputs) = &task.outputs else {
      return Ok(());
    };
    let mut field_secrecy = HashMap::new();
    for (name, output) in outputs {
      if name.trim().is_empty() || output.step.trim().is_empty() || output.field.trim().is_empty() {
        return Err("output names, steps, and fields must not be empty".to_owned());
      }
      let producers = steps
        .get(output.step.as_str())
        .ok_or_else(|| format!("output '{name}' references unknown plugin step '{}'", output.step))?;
      if producers.iter().any(|(_, deferred)| *deferred) {
        return Err(format!("output '{name}' references deferred step '{}'", output.step));
      }
      if producers
        .iter()
        .any(|(plugin_key, _)| !self.plugins[*plugin_key].required_output_fields.contains(&output.field))
      {
        return Err(format!(
          "output '{name}' references field '{}' that is undeclared or optional on a producer of plugin step '{}'",
          output.field, output.step
        ));
      }
      if field_secrecy
        .insert((output.step.as_str(), output.field.as_str()), output.secret)
        .is_some_and(|secret| secret != output.secret)
      {
        return Err(format!(
          "plugin field '{}.{}' cannot be exported as both secret and public",
          output.step, output.field
        ));
      }
    }
    Ok(())
  }
}

fn required_output_fields(
  key: &str,
  schema: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<HashSet<String>, String> {
  let Some(schema) = schema else {
    return Ok(HashSet::new());
  };
  jsonschema::validator_for(&serde_json::Value::Object(schema.clone()))
    .map_err(|error| format!("invalid output schema for plugin '{key}': {error}"))?;
  if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
    return Err(format!("output schema for plugin '{key}' must have type 'object'"));
  }
  if schema.get("additionalProperties").and_then(serde_json::Value::as_bool) != Some(false) {
    return Err(format!(
      "output schema for plugin '{key}' must set additionalProperties to false"
    ));
  }
  let properties = schema
    .get("properties")
    .and_then(serde_json::Value::as_object)
    .ok_or_else(|| format!("output schema for plugin '{key}' must declare object properties"))?;
  let required = schema
    .get("required")
    .and_then(serde_json::Value::as_array)
    .map(|fields| {
      fields
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect::<HashSet<_>>()
    })
    .unwrap_or_default();
  Ok(
    properties
      .keys()
      .filter(|field| required.contains(*field))
      .cloned()
      .collect(),
  )
}

fn validate_plugin_key(key: &str) -> Result<(), String> {
  if key.is_empty() {
    return Err("plugin task key cannot be empty".to_string());
  }
  if RESERVED_PLUGIN_KEYS.contains(&key) {
    return Err(format!("plugin task key '{key}' is reserved by Octafile syntax"));
  }

  Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct Task {
  pub env: Option<Envs>,                             // Task environment variables
  pub dotenv: Option<Vec<String>>,                   // Environment files applied to this task
  pub dir: Option<PathBuf>,                          // Working directory for the task
  pub desc: Option<String>,                          // Task description
  pub prefix: Option<String>,                        // Label used by prefixed output
  pub presentation: Option<TaskPresentation>,        // Task-specific output presentation
  pub vars: Option<Vars>,                            // Task-specific variables
  pub cmds: Option<Vec<TaskCommand>>,                // List of commands
  pub internal: Option<bool>,                        // Show command in list of available commands
  pub platforms: Option<Vec<String>>,                // Supported platforms
  pub ignore_error: Option<bool>,                    // Whether to continue on error
  pub deps: Option<Vec<Deps>>,                       // Task dependencies
  pub run: Option<AllowedRun>,                       // When task should run
  pub quiet: Option<bool>,                           // Suppress Octa's task diagnostics
  pub silent: Option<Silence>,                       // Suppress one or both task streams
  pub raw: Option<bool>,                             // Use exclusive byte-oriented terminal IO
  pub interactive: Option<bool>,                     // Reserve terminal IO for the whole task body
  pub execute_mode: Option<ExecuteMode>,             // How execute task commands
  pub failfast: Option<bool>,                        // Cancel parallel work after the first failure
  pub timeout: Option<Timeout>,                      // Default timeout for task commands
  pub sources: Option<Vec<String>>,                  // Sources for fingerprinting
  pub output: Option<Vec<String>>,                   // Files produced by this task
  pub outputs: Option<IndexMap<String, TaskOutput>>, // Structured values exported from named steps
  pub artifacts: Option<Vec<ArtifactDeclaration>>,   // Files/directories registered after this task
  pub reports: Option<Vec<ReportDeclaration>>,       // Machine-readable reports registered after this task
  pub source_strategy: Option<SourceStrategies>,     // Strategy used to fingerprint sources
  pub watch: Option<bool>,                           // Watch sources and rerun the task
  pub condition: Option<TaskConditions>,             // Plugin conditions around dependency execution
  pub preconditions: Option<Vec<String>>,            // Commands to check should run command
  pub plugin: Option<PluginCommand>,                 // Plugin command executed by this task
}

pub(crate) struct TaskSeed<'a> {
  pub(crate) context: &'a Context,
}

impl<'de> DeserializeSeed<'de> for TaskSeed<'_> {
  type Value = Task;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(TaskVisitor { context: self.context })
  }
}

struct TaskVisitor<'a> {
  context: &'a Context,
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
    let cmd_value = Value::String(value.to_string());
    let default_plugin = self.context.default_plugin();
    self.context.validate(default_plugin, &cmd_value).map_err(E::custom)?;

    Ok(Task {
      plugin: Some(PluginCommand {
        key: default_plugin.to_owned(),
        value: cmd_value,
      }),
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
        "prefix" => task.prefix = map.next_value()?,
        "presentation" => task.presentation = map.next_value()?,
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
        "quiet" => task.quiet = map.next_value()?,
        "silent" => task.silent = map.next_value()?,
        "raw" => task.raw = map.next_value()?,
        "interactive" => task.interactive = map.next_value()?,
        "execute_mode" => task.execute_mode = map.next_value()?,
        "failfast" => task.failfast = map.next_value()?,
        "timeout" => task.timeout = map.next_value()?,
        "sources" => task.sources = map.next_value()?,
        "output" => task.output = map.next_value()?,
        "outputs" => task.outputs = map.next_value()?,
        "artifacts" => task.artifacts = map.next_value()?,
        "reports" => task.reports = map.next_value()?,
        "source_strategy" => task.source_strategy = map.next_value()?,
        "watch" => task.watch = map.next_value()?,
        "if" => {
          let condition: Option<Value> = map.next_value()?;
          task.condition = condition
            .map(|condition| self.context.parse_task_conditions(condition))
            .transpose()
            .map_err(serde::de::Error::custom)?;
        },
        "preconditions" => task.preconditions = map.next_value()?,
        key => {
          if !self.context.contains(key) {
            return Err(serde::de::Error::custom(format!("unknown task field '{key}'")));
          }

          if task.plugin.is_some() {
            return Err(serde::de::Error::custom(
              "a task cannot define more than one plugin task type",
            ));
          }

          let value = map.next_value()?;
          self.context.validate(key, &value).map_err(serde::de::Error::custom)?;
          task.plugin = Some(PluginCommand {
            key: key.to_owned(),
            value,
          });
        },
      }
    }

    if task.plugin.is_some() && task.cmds.is_some() {
      return Err(serde::de::Error::custom(
        "a task cannot define both 'cmds' and a plugin task type",
      ));
    }

    self
      .context
      .validate_task_outputs(&task)
      .map_err(serde::de::Error::custom)?;

    Ok(task)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn context() -> Context {
    Context::from_keys(vec!["shell".to_owned(), "tpl".to_owned()], "shell").unwrap()
  }

  fn structured_context() -> Context {
    Context::from_schemas(
      PluginSchemas::from([(
        "docker".to_owned(),
        PluginTypeSchema {
          input: None,
          output: serde_json::json!({
            "type": "object",
            "properties": {
              "digest": { "type": "string" },
              "pushed": { "type": "boolean" }
            },
            "required": ["digest"],
            "additionalProperties": false
          })
          .as_object()
          .cloned(),
        },
      )]),
      "docker",
    )
    .unwrap()
  }

  fn yaml_value(content: &str) -> Value {
    serde_yml::from_str(content).unwrap()
  }

  fn parse_task(context: &Context, content: &str) -> Result<Task, String> {
    let value = yaml_value(content);
    TaskSeed { context }
      .deserialize(serde_yml::Deserializer::new(&value))
      .map_err(|error| error.to_string())
  }

  #[test]
  fn both_context_constructors_reject_invalid_plugin_keys() {
    let empty_key_error = Context::from_keys(vec![String::new()], "").err().unwrap();
    assert_eq!(empty_key_error, "plugin task key cannot be empty");

    for key in RESERVED_PLUGIN_KEYS {
      let error = Context::from_keys(vec![(*key).to_string()], *key).err().unwrap();
      assert_eq!(error, format!("plugin task key '{key}' is reserved by Octafile syntax"));
    }

    let schemas = PluginSchemas::from([("timeout".to_string(), PluginTypeSchema::default())]);
    let schema_error = Context::from_schemas(schemas, "timeout").err().unwrap();
    assert_eq!(schema_error, "plugin task key 'timeout' is reserved by Octafile syntax");
  }

  #[test]
  fn key_only_context_rejects_duplicate_plugin_keys() {
    let error = Context::from_keys(vec!["shell".to_string(), "shell".to_string()], "shell")
      .err()
      .unwrap();

    assert_eq!(error, "duplicated plugin task key 'shell'");
  }

  #[test]
  fn validates_named_step_exports_against_plugin_output_schema() {
    let task = parse_task(
      &structured_context(),
      r#"
cmds:
  - id: build
    docker: { image: app }
outputs:
  image_digest:
    step: build
    field: digest
"#,
    )
    .unwrap();

    assert_eq!(task.cmds.unwrap()[0].options.id.as_deref(), Some("build"));
    assert_eq!(task.outputs.unwrap()["image_digest"].field, "digest");

    let error = parse_task(
      &structured_context(),
      r#"
cmds:
  - id: build
    docker: { image: app }
outputs:
  image_id: { step: build, field: unknown }
"#,
    )
    .unwrap_err();
    assert!(
      error.contains("field 'unknown' that is undeclared or optional"),
      "{error}"
    );

    let error = parse_task(
      &Context::from_schemas(
        PluginSchemas::from([(
          "docker".to_owned(),
          PluginTypeSchema {
            input: None,
            output: serde_json::json!({
              "type": "object",
              "properties": { "digest": { "type": "string" } },
              "additionalProperties": false
            })
            .as_object()
            .cloned(),
          },
        )]),
        "docker",
      )
      .unwrap(),
      "cmds:\n  - { id: build, docker: run }\noutputs:\n  value: { step: build, field: digest }",
    )
    .unwrap_err();
    assert!(error.contains("undeclared or optional"), "{error}");
  }

  #[test]
  fn accepts_secret_exports_and_rejects_mixed_visibility_for_one_plugin_field() {
    let task = parse_task(
      &structured_context(),
      "cmds:\n  - { id: build, docker: run }\noutputs:\n  token: { step: build, field: digest, secret: true }",
    )
    .unwrap();
    assert!(task.outputs.unwrap()["token"].secret);

    let error = parse_task(
      &structured_context(),
      "cmds:\n  - { id: build, docker: run }\noutputs:\n  public: { step: build, field: digest }\n  token: { step: build, field: digest, secret: true }",
    )
    .unwrap_err();
    assert!(error.contains("both secret and public"), "{error}");
  }

  #[test]
  fn allows_one_output_step_to_have_platform_specific_producers() {
    let task = parse_task(
      &structured_context(),
      "cmds:\n  - { id: build, platforms: [linux], docker: linux }\n  - { id: build, platforms: [windows], docker: windows }\noutputs:\n  digest: { step: build, field: digest }",
    )
    .unwrap();

    assert_eq!(task.cmds.unwrap().len(), 2);
  }

  #[test]
  fn rejects_ambiguous_or_unavailable_output_steps() {
    for (yaml, expected) in [
      (
        "cmds:\n  - { id: cleanup, defer: { docker: clean } }\noutputs:\n  value: { step: cleanup, field: digest }",
        "references deferred step 'cleanup'",
      ),
      (
        "cmds:\n  - { id: build, docker: run }\noutputs:\n  value: { step: missing, field: digest }",
        "unknown plugin step 'missing'",
      ),
      (
        "sources: [src]\ncmds:\n  - { id: build, docker: run }\noutputs:\n  value: { step: build, field: digest }",
        "cannot be combined with freshness",
      ),
      ("cmds:\n  - { id: '', docker: run }", "command id must not be empty"),
      ("cmds:\n  - { id: nested, task: child }", "belongs to a task reference"),
      (
        "cmds:\n  - { id: build, docker: run }\noutputs:\n  '': { step: build, field: digest }",
        "output names, steps, and fields must not be empty",
      ),
    ] {
      let error = parse_task(&structured_context(), yaml).unwrap_err();
      assert!(error.contains(expected), "{error}");
    }
  }

  #[test]
  fn an_explicit_empty_structured_output_map_does_not_disable_file_freshness() {
    parse_task(
      &structured_context(),
      "sources: [src]\noutput: [target]\noutputs: {}\ndocker: build",
    )
    .unwrap();
  }

  #[test]
  fn output_schemas_must_expose_a_closed_object_shape() {
    for schema in [
      serde_json::json!({ "type": "string" }),
      serde_json::json!({ "type": "object", "additionalProperties": false }),
      serde_json::json!({ "type": "object", "properties": {} }),
    ] {
      let result = Context::from_schemas(
        PluginSchemas::from([(
          "docker".to_owned(),
          PluginTypeSchema {
            input: None,
            output: schema.as_object().cloned(),
          },
        )]),
        "docker",
      );
      assert!(result.is_err());
    }
  }

  #[test]
  fn serializes_timeouts_and_converts_public_shorthand_types() {
    let timeout: Timeout = serde_yml::from_str("2s").unwrap();
    assert_eq!(timeout.duration(), Duration::from_secs(2));
    assert_eq!(
      serde_yml::from_str::<Timeout>(&serde_yml::to_string(&timeout).unwrap()).unwrap(),
      timeout
    );

    assert_eq!(ExecuteMode::from("parallel".to_owned()), ExecuteMode::Parallel);
    assert_eq!(ExecuteMode::from("sequentially".to_owned()), ExecuteMode::Sequentially);
    assert_eq!(
      SourceStrategies::from("timestamp".to_owned()),
      SourceStrategies::Timestamp
    );
    assert_eq!(SourceStrategies::from("hash".to_owned()), SourceStrategies::Hash);
    assert_eq!(
      SourceStrategies::from("content-addressed".to_owned()),
      SourceStrategies::Custom("content-addressed".to_owned())
    );
    assert_eq!(serde_yml::to_string(&SourceStrategies::Hash).unwrap().trim(), "hash");
    assert_eq!(
      serde_yml::to_string(&SourceStrategies::Custom("content-addressed".to_owned()))
        .unwrap()
        .trim(),
      "content-addressed"
    );
    assert_eq!(AllowedRun::from("once".to_owned()), AllowedRun::Once);
    assert_eq!(AllowedRun::from("always".to_owned()), AllowedRun::Always);
    assert_eq!(AllowedRun::from("changed".to_owned()), AllowedRun::Changed);
    assert!(matches!(Deps::from("build".to_owned()), Deps::Simple(task) if task == "build"));
  }

  #[test]
  fn rejects_invalid_condition_shapes() {
    let context = context();

    let error = context
      .parse_condition_command(yaml_value("unknown: command"))
      .unwrap_err();
    assert_eq!(error, "unknown plugin condition type 'unknown'");

    let error = context
      .parse_task_conditions(yaml_value("before_deps: echo check\nunexpected: value"))
      .unwrap_err();
    assert_eq!(error, "unknown task condition field 'unexpected'");

    assert_eq!(
      context.parse_task_conditions(yaml_value("[one, two]")).unwrap_err(),
      "task condition must be a string or a mapping"
    );
  }

  #[test]
  fn rejects_ambiguous_and_unknown_commands() {
    let context = context();
    let cases = [
      ("1", "commands must be strings, task references, or plugin commands"),
      (
        "shell: one\ntpl: two",
        "a plugin command must contain exactly one plugin task type",
      ),
      ("unknown: command", "unknown plugin command type 'unknown'"),
      (
        "defer:\n  shell: cleanup\nshell: build",
        "a deferred command cannot contain sibling command fields",
      ),
      (
        "defer: [one, two]",
        "commands must be strings, task references, or plugin commands",
      ),
    ];

    for (yaml, expected) in cases {
      assert_eq!(context.parse_command(yaml_value(yaml)).unwrap_err(), expected);
    }

    assert!(context
      .parse_command(yaml_value("task: build\nunknown: value"))
      .unwrap_err()
      .starts_with("invalid task command:"));
  }

  #[test]
  fn task_seed_rejects_invalid_shapes_and_conflicting_payloads() {
    let context = context();

    let error = parse_task(&context, "[one, two]").unwrap_err();
    assert!(error.contains("a string or a map representing a Task"));

    let error = parse_task(&context, "cmds: [echo one]\nshell: echo two").unwrap_err();
    assert!(error.contains("a task cannot define both 'cmds' and a plugin task type"));
  }

  #[test]
  fn parses_a_custom_output_prefix() {
    let task = parse_task(&context(), "prefix: api\nshell: echo ready").unwrap();

    assert_eq!(task.prefix.as_deref(), Some("api"));
  }

  #[test]
  fn parses_task_specific_presentation_without_colliding_with_artifact_outputs() {
    let task = parse_task(
      &context(),
      "presentation:\n  output: replacing\noutput: [dist/app]\nshell: echo ready",
    )
    .unwrap();

    assert_eq!(
      task.presentation.and_then(|presentation| presentation.output),
      Some(crate::TaskOutputMode::Replacing)
    );
    assert_eq!(task.output, Some(vec!["dist/app".to_owned()]));
  }

  #[test]
  fn parses_task_wide_interactive_mode() {
    let task = parse_task(&context(), "interactive: true\nshell: cargo test -- --nocapture").unwrap();

    assert_eq!(task.interactive, Some(true));
  }

  #[test]
  fn silence_parses_serializes_and_reports_hidden_streams() {
    let values = [
      (Silence::None, "false", false, false),
      (Silence::All, "true", true, true),
      (Silence::Stdout, "stdout", true, false),
      (Silence::Stderr, "stderr", false, true),
    ];

    for (silence, serialized, hides_stdout, hides_stderr) in values {
      assert_eq!(serde_yml::to_string(&silence).unwrap().trim(), serialized);
      assert_eq!(silence.hides_stdout(), hides_stdout);
      assert_eq!(silence.hides_stderr(), hides_stderr);
    }

    assert_eq!(Silence::from(false), Silence::None);
    assert_eq!(Silence::from(true), Silence::All);
    assert_eq!("0".parse(), Ok(Silence::None));
    assert_eq!("1".parse(), Ok(Silence::All));
    assert!("quiet".parse::<Silence>().is_err());
    assert!(serde_yml::from_str::<Silence>("quiet").is_err());
  }
}
