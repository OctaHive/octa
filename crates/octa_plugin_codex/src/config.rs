//! Codex task configuration and semantic validation.
//!
//! This module is the only boundary that turns an untrusted plugin value into
//! typed task intent. JSON Schema rejects malformed Octafiles early, while
//! [`CodexConfig::parse`] repeats security-relevant bounds before any file is
//! read or process is started. The other modules receive only a validated
//! configuration and do not reinterpret paths or variable mappings.

use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Context};
use serde::Deserialize;
use serde_json::{Map, Value};

/// Maximum UTF-8 size of an inline prompt.
const MAX_PROMPT_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 size of a complete portable workspace-relative path.
const MAX_PATH_BYTES: usize = 4 * 1024;
/// Maximum encoded size reserved for the unique invocation directory name.
pub(crate) const MAX_INVOCATION_COMPONENT_BYTES: usize = 128;
/// Maximum encoded size reserved for any plugin-owned record file name.
pub(crate) const MAX_RECORD_FILE_NAME_BYTES: usize = 64;
/// A record root must leave room for `/<invocation>/<record-file>`.
const MAX_RUN_RECORDS_BYTES: usize = MAX_PATH_BYTES - 2 - MAX_INVOCATION_COMPONENT_BYTES - MAX_RECORD_FILE_NAME_BYTES;
/// Maximum UTF-8 size of a model name or variable reference.
const MAX_NAME_BYTES: usize = 256;
/// Maximum UTF-8 size of an optional source revision.
const MAX_SOURCE_REVISION_BYTES: usize = 4 * 1024;
/// Maximum serialized size of a user-provided result schema.
const MAX_RESULT_SCHEMA_BYTES: usize = 256 * 1024;
/// Maximum number of public and secret child-environment mappings combined.
const MAX_ENVIRONMENT_MAPPINGS: usize = 64;
/// Maximum number of resources registered by one Codex invocation.
const MAX_DELIVERABLES: usize = 64;
const DEFAULT_RUN_RECORDS: &str = ".octa/codex-runs";

/// Validated task intent consumed by the invocation builder.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexConfig {
  pub(crate) prompt: Option<String>,
  pub(crate) prompt_file: Option<String>,
  pub(crate) model: Option<String>,
  #[cfg_attr(
    not(test),
    expect(
      dead_code,
      reason = "task 3.1 consumes the validated setting when building CLI arguments"
    )
  )]
  pub(crate) reasoning_effort: Option<ReasoningEffort>,
  pub(crate) result_schema: Option<Map<String, Value>>,
  #[serde(default = "default_run_records")]
  pub(crate) run_records: String,
  #[serde(default)]
  pub(crate) environment: EnvironmentSelection,
  pub(crate) source_revision: Option<String>,
  #[serde(default)]
  pub(crate) deliverables: Vec<Deliverable>,
}

/// Model reasoning effort passed to the concrete Codex CLI adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReasoningEffort {
  None,
  Minimal,
  Low,
  Medium,
  High,
  Xhigh,
  Max,
}

/// Explicit mappings from child environment names to Octa variable names.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentSelection {
  #[serde(default)]
  pub(crate) public: BTreeMap<String, String>,
  #[serde(default)]
  pub(crate) secret: BTreeMap<String, String>,
}

/// Exact artifact or report expected after a successful Codex run.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Deliverable {
  Artifact {
    name: String,
    path: String,
    content_type: Option<String>,
  },
  Report {
    name: String,
    path: String,
    format: String,
  },
}

impl CodexConfig {
  /// Parses and validates a plugin value without consulting process or filesystem state.
  pub(crate) fn parse(value: Value) -> anyhow::Result<Self> {
    let result: Self = serde_json::from_value(value).context("invalid Codex task configuration")?;
    result.validate()?;
    Ok(result)
  }

  fn validate(&self) -> anyhow::Result<()> {
    match (&self.prompt, &self.prompt_file) {
      (Some(prompt), None) => validate_prompt(prompt)?,
      (None, Some(path)) => validate_path("prompt_file", path)?,
      (Some(_), Some(_)) => bail!("exactly one of prompt or prompt_file must be configured"),
      (None, None) => bail!("one of prompt or prompt_file must be configured"),
    }

    if let Some(model) = &self.model {
      validate_name("model", model)?;
    }
    if let Some(schema) = &self.result_schema {
      validate_result_schema(schema)?;
    }
    validate_path_with_limit("run_records", &self.run_records, MAX_RUN_RECORDS_BYTES)?;
    self.environment.validate()?;
    if let Some(revision) = &self.source_revision {
      validate_bounded_text("source_revision", revision, MAX_SOURCE_REVISION_BYTES)?;
    }
    validate_deliverables(&self.deliverables)
  }
}

impl EnvironmentSelection {
  fn validate(&self) -> anyhow::Result<()> {
    if self.public.len() + self.secret.len() > MAX_ENVIRONMENT_MAPPINGS {
      bail!("environment mappings are limited to {MAX_ENVIRONMENT_MAPPINGS} entries");
    }

    for (environment_name, variable_name) in self.public.iter().chain(&self.secret) {
      validate_environment_name(environment_name)?;
      validate_name("Octa variable name", variable_name)?;
    }
    if let Some(name) = self.public.keys().find(|name| self.secret.contains_key(*name)) {
      bail!("child environment variable '{name}' cannot be both public and secret");
    }
    Ok(())
  }
}

fn validate_deliverables(deliverables: &[Deliverable]) -> anyhow::Result<()> {
  if deliverables.len() > MAX_DELIVERABLES {
    bail!("deliverables are limited to {MAX_DELIVERABLES} entries");
  }

  let mut names = HashSet::with_capacity(deliverables.len());
  for deliverable in deliverables {
    let (name, path) = match deliverable {
      Deliverable::Artifact {
        name,
        path,
        content_type,
      } => {
        if let Some(content_type) = content_type {
          validate_name("artifact content type", content_type)?;
        }
        (name, path)
      },
      Deliverable::Report { name, path, format } => {
        validate_name("report format", format)?;
        (name, path)
      },
    };
    validate_name("deliverable name", name)?;
    validate_path("deliverable path", path)?;
    if !names.insert(name) {
      bail!("deliverable name '{name}' is configured more than once");
    }
  }
  Ok(())
}

fn validate_prompt(prompt: &str) -> anyhow::Result<()> {
  if prompt.trim().is_empty() {
    bail!("prompt must not be empty");
  }
  if prompt.len() > MAX_PROMPT_BYTES {
    bail!("prompt exceeds {MAX_PROMPT_BYTES} UTF-8 bytes");
  }
  if prompt.contains('\0') {
    bail!("prompt must not contain NUL characters");
  }
  Ok(())
}

fn validate_path(kind: &str, path: &str) -> anyhow::Result<()> {
  validate_path_with_limit(kind, path, MAX_PATH_BYTES)
}

fn validate_path_with_limit(kind: &str, path: &str, maximum: usize) -> anyhow::Result<()> {
  if path.is_empty() || path.len() > maximum {
    bail!("{kind} must contain 1 to {maximum} UTF-8 bytes");
  }
  if path.starts_with('/') || path.contains(['\\', ':']) {
    bail!("{kind} must be portable, relative, and use '/' separators");
  }
  if path.chars().any(char::is_control) {
    bail!("{kind} must not contain control characters");
  }
  for component in path.split('/') {
    if component.is_empty() || component == "." || component == ".." {
      bail!("{kind} must be normalized and remain below the workspace root");
    }
    if component.ends_with(['.', ' '])
      || component
        .chars()
        .any(|character| matches!(character, '<' | '>' | '"' | '|' | '?' | '*'))
      || is_windows_device_name(component)
    {
      bail!("{kind} contains a component that is not portable to Windows");
    }
  }
  Ok(())
}

fn is_windows_device_name(component: &str) -> bool {
  let stem = component.split('.').next().unwrap_or_default();
  stem.eq_ignore_ascii_case("con")
    || stem.eq_ignore_ascii_case("prn")
    || stem.eq_ignore_ascii_case("aux")
    || stem.eq_ignore_ascii_case("nul")
    || matches_windows_numbered_device(stem, "com")
    || matches_windows_numbered_device(stem, "lpt")
}

fn matches_windows_numbered_device(value: &str, prefix: &str) -> bool {
  let value = value.as_bytes();
  let prefix = prefix.as_bytes();
  value.len() == prefix.len() + 1
    && value[..prefix.len()].eq_ignore_ascii_case(prefix)
    && matches!(value[prefix.len()], b'1'..=b'9')
}

fn validate_environment_name(name: &str) -> anyhow::Result<()> {
  let mut characters = name.chars();
  let valid_start = characters
    .next()
    .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
  if !valid_start || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric()) {
    bail!("child environment name is not portable");
  }
  if name.len() > MAX_NAME_BYTES {
    bail!("child environment name exceeds {MAX_NAME_BYTES} UTF-8 bytes");
  }
  Ok(())
}

fn validate_name(kind: &str, value: &str) -> anyhow::Result<()> {
  validate_bounded_text(kind, value, MAX_NAME_BYTES)
}

fn validate_bounded_text(kind: &str, value: &str, maximum: usize) -> anyhow::Result<()> {
  if value.is_empty() || value.trim() != value || value.len() > maximum || value.chars().any(char::is_control) {
    bail!("{kind} must contain 1 to {maximum} UTF-8 bytes without surrounding whitespace or control characters");
  }
  Ok(())
}

fn validate_result_schema(schema: &Map<String, Value>) -> anyhow::Result<()> {
  let schema = Value::Object(schema.clone());
  let bytes = serde_json::to_vec(&schema).context("failed to encode result_schema")?;
  if bytes.len() > MAX_RESULT_SCHEMA_BYTES {
    bail!("result_schema exceeds {MAX_RESULT_SCHEMA_BYTES} encoded bytes");
  }
  jsonschema::validator_for(&schema).context("result_schema is not a valid JSON Schema")?;
  Ok(())
}

fn default_run_records() -> String {
  DEFAULT_RUN_RECORDS.to_owned()
}

/// Returns the strict task-input schema advertised through the plugin protocol.
pub(crate) fn input_schema() -> Map<String, Value> {
  serde_json::json!({
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "additionalProperties": false,
    "properties": {
      "prompt": prompt_schema(),
      "prompt_file": path_schema(),
      "model": bounded_name_schema(),
      "reasoning_effort": {
        "type": "string",
        "enum": ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
      },
      "result_schema": { "type": "object" },
      "run_records": path_schema_with_maximum(MAX_RUN_RECORDS_BYTES),
      "environment": environment_schema(),
      "source_revision": bounded_text_schema(MAX_SOURCE_REVISION_BYTES),
      "deliverables": {
        "type": "array",
        "maxItems": MAX_DELIVERABLES,
        "items": deliverable_schema()
      }
    },
    "oneOf": [
      {
        "required": ["prompt"],
        "not": { "required": ["prompt_file"] }
      },
      {
        "required": ["prompt_file"],
        "not": { "required": ["prompt"] }
      }
    ]
  })
  .as_object()
  .cloned()
  .expect("Codex input schema is an object")
}

/// Returns the portable workspace-relative path shape shared by task input
/// and plugin-owned output metadata.
pub(crate) fn path_schema() -> Value {
  path_schema_with_maximum(MAX_PATH_BYTES)
}

fn path_schema_with_maximum(maximum: usize) -> Value {
  serde_json::json!({
    "type": "string",
    "minLength": 1,
    "maxLength": maximum,
    "allOf": [
      { "not": { "pattern": "^/" } },
      { "not": { "pattern": "(^|/)\\.\\.?(/|$)" } },
      { "not": { "pattern": "//|/$" } },
      { "not": { "pattern": "[\\\\:]" } },
      { "not": { "pattern": "[\\u0000-\\u001f\\u007f]" } },
      { "not": { "pattern": "[<>\"|?*]" } },
      { "not": { "pattern": "[. ](/|$)" } },
      {
        "not": {
          "pattern": "(^|/)([Cc][Oo][Nn]|[Pp][Rr][Nn]|[Aa][Uu][Xx]|[Nn][Uu][Ll]|[Cc][Oo][Mm][1-9]|[Ll][Pp][Tt][1-9])(\\.[^/]*)?(/|$)"
        }
      }
    ]
  })
}

fn prompt_schema() -> Value {
  serde_json::json!({
    "type": "string",
    "minLength": 1,
    "maxLength": MAX_PROMPT_BYTES,
    "pattern": "\\S",
    "not": { "pattern": "\\u0000" }
  })
}

fn bounded_name_schema() -> Value {
  bounded_text_schema(MAX_NAME_BYTES)
}

fn bounded_text_schema(maximum: usize) -> Value {
  serde_json::json!({
    "type": "string",
    "minLength": 1,
    "maxLength": maximum,
    "pattern": "^\\S(?:.*\\S)?$",
    "not": { "pattern": "[\\u0000-\\u001f\\u007f]" }
  })
}

fn environment_schema() -> Value {
  let mapping = serde_json::json!({
    "type": "object",
    "maxProperties": MAX_ENVIRONMENT_MAPPINGS,
    "propertyNames": {
      "pattern": "^[A-Za-z_][A-Za-z0-9_]*$",
      "maxLength": MAX_NAME_BYTES
    },
    "additionalProperties": bounded_name_schema()
  });
  serde_json::json!({
    "type": "object",
    "additionalProperties": false,
    "properties": {
      "public": mapping,
      "secret": mapping
    }
  })
}

fn deliverable_schema() -> Value {
  serde_json::json!({
    "oneOf": [
      {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "kind": { "const": "artifact" },
          "name": bounded_name_schema(),
          "path": path_schema(),
          "content_type": bounded_name_schema()
        },
        "required": ["kind", "name", "path"]
      },
      {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "kind": { "const": "report" },
          "name": bounded_name_schema(),
          "path": path_schema(),
          "format": bounded_name_schema()
        },
        "required": ["kind", "name", "path", "format"]
      }
    ]
  })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
