//! Validation contracts for durable run records and plugin outputs.
//!
//! The module defines the shapes and bounded sizes that the record writer and
//! protocol boundary must enforce. It neither interprets task configuration
//! nor controls the Codex process.

use std::sync::OnceLock;

use anyhow::bail;
use serde_json::{Map, Value};

use crate::config;

mod limits;
use limits::{
  validate_encoded_size, MAX_FINAL_MESSAGE_BYTES, MAX_PLUGIN_OUTPUT_BYTES, MAX_STRUCTURED_RESULT_BYTES,
  MAX_USAGE_METADATA_BYTES,
};

/// Maximum number of characters advertised for a plain final message.
///
/// JSON Schema counts Unicode characters rather than encoded bytes. Runtime
/// retention additionally enforces [`MAX_FINAL_MESSAGE_BYTES`], so this is a
/// portable upper bound rather than a second independently chosen limit.
const MAX_FINAL_MESSAGE_CHARACTERS: usize = MAX_FINAL_MESSAGE_BYTES;
/// Maximum number of harness identifiers retained for one invocation.
const MAX_HARNESS_IDENTIFIERS: usize = 16;
/// Maximum number of characters retained in one harness identifier.
const MAX_HARNESS_IDENTIFIER_LENGTH: usize = 1024;
/// Maximum number of usage counters retained for one invocation.
const MAX_USAGE_COUNTERS: usize = 32;
/// Maximum portable name length for an identifier or usage counter.
const MAX_OUTPUT_NAME_LENGTH: usize = 128;
static OUTPUT_VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();

/// Returns the successful-completion schema advertised by the Codex plugin.
///
/// A well-formed harness terminal state is an orchestration success even when
/// its semantic outcome is `blocked` or `failed`. Callers therefore gate on
/// `outcome` instead of inferring success from the process code. Exactly one of
/// `final_message` and `structured_result` is present: configured structured
/// output is never guessed from prose. Empty bounded maps represent identifiers
/// or counters that the selected harness version did not report.
///
/// `structured_result` deliberately preserves the JSON type accepted by the
/// task-provided result schema. [`validate_outputs`] enforces its encoded byte
/// limit because JSON Schema cannot express a total encoded-size limit for an
/// arbitrary nested value.
///
/// The single-field `{ "outcome": "completed" }` form is reserved for static
/// dry-run validation. It avoids fabricating harness metadata or record paths
/// for an invocation that deliberately performed no execution or file writes.
pub(crate) fn output_schema() -> Map<String, Value> {
  serde_json::json!({
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "additionalProperties": false,
    "properties": {
      "outcome": {
        "type": "string",
        "enum": ["completed", "blocked", "needs_input", "budget_exhausted", "failed"]
      },
      "final_message": {
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_FINAL_MESSAGE_CHARACTERS
      },
      "structured_result": true,
      "harness_identifiers": bounded_string_map_schema(
        MAX_HARNESS_IDENTIFIERS,
        MAX_HARNESS_IDENTIFIER_LENGTH,
      ),
      "usage": {
        "type": "object",
        "maxProperties": MAX_USAGE_COUNTERS,
        "propertyNames": output_name_schema(),
        "additionalProperties": {
          "type": "integer",
          "minimum": 0,
          "maximum": u64::MAX
        }
      },
      "record_paths": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "trace": config::path_schema(),
          "result": config::path_schema(),
          "provenance": config::path_schema()
        },
        "required": ["trace", "result", "provenance"]
      }
    },
    "required": ["outcome"],
    "oneOf": [
      {
        "properties": { "outcome": { "const": "completed" } },
        "maxProperties": 1
      },
      {
        "required": ["final_message", "harness_identifiers", "usage", "record_paths"],
        "not": { "required": ["structured_result"] }
      },
      {
        "required": ["structured_result", "harness_identifiers", "usage", "record_paths"],
        "not": { "required": ["final_message"] }
      }
    ]
  })
  .as_object()
  .cloned()
  .expect("Codex output schema is an object")
}

/// Validates one completed output before it crosses the plugin protocol.
///
/// Shape constraints remain in the advertised JSON Schema so Octa can reject
/// invalid output independently. Encoded-size checks live here because they
/// bound arbitrary nested JSON and UTF-8 bytes rather than JSON character or
/// collection counts.
pub(crate) fn validate_outputs(outputs: &Map<String, Value>) -> anyhow::Result<()> {
  validate_encoded_size("Codex output", outputs, MAX_PLUGIN_OUTPUT_BYTES)?;

  if outputs
    .get("final_message")
    .and_then(Value::as_str)
    .is_some_and(|message| message.len() > MAX_FINAL_MESSAGE_BYTES)
  {
    bail!("final message exceeds {MAX_FINAL_MESSAGE_BYTES} UTF-8 bytes");
  }
  if let Some(structured_result) = outputs.get("structured_result") {
    validate_encoded_size("structured_result", structured_result, MAX_STRUCTURED_RESULT_BYTES)?;
  }
  if let Some(usage) = outputs.get("usage") {
    validate_encoded_size("usage metadata", usage, MAX_USAGE_METADATA_BYTES)?;
  }

  let value = Value::Object(outputs.clone());
  let validator = OUTPUT_VALIDATOR.get_or_init(|| {
    jsonschema::validator_for(&Value::Object(output_schema())).expect("Codex output schema must compile")
  });
  if !validator.is_valid(&value) {
    bail!("Codex output does not satisfy the advertised schema");
  }
  Ok(())
}

/// Returns the only successful output that does not represent a harness run.
pub(crate) fn dry_run_outputs() -> Map<String, Value> {
  Map::from_iter([("outcome".to_owned(), Value::String("completed".to_owned()))])
}

fn bounded_string_map_schema(maximum_entries: usize, maximum_value_length: usize) -> Value {
  serde_json::json!({
    "type": "object",
    "maxProperties": maximum_entries,
    "propertyNames": output_name_schema(),
    "additionalProperties": {
      "type": "string",
      "minLength": 1,
      "maxLength": maximum_value_length,
      "not": { "pattern": "[\\u0000-\\u001f\\u007f]" }
    }
  })
}

fn output_name_schema() -> Value {
  serde_json::json!({
    "type": "string",
    "minLength": 1,
    "maxLength": MAX_OUTPUT_NAME_LENGTH,
    "pattern": "^[a-z][a-z0-9_]*$"
  })
}

#[cfg(test)]
#[path = "records_tests.rs"]
mod tests;
