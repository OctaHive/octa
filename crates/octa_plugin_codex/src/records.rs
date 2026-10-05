//! Sanitized, durable run-record construction and resource declarations.
//!
//! This module writes trace, result, and provenance data only after receiving
//! normalized events. It neither interprets task configuration nor controls
//! the Codex process.

use std::{io, sync::OnceLock};

use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config;

/// Maximum number of characters retained in a plain final message.
///
/// The event reader also applies a byte limit before constructing this value;
/// the schema limit protects every consumer of the advertised plugin output.
const MAX_FINAL_MESSAGE_LENGTH: usize = 256 * 1024;
/// Maximum number of harness identifiers retained for one invocation.
const MAX_HARNESS_IDENTIFIERS: usize = 16;
/// Maximum number of characters retained in one harness identifier.
const MAX_HARNESS_IDENTIFIER_LENGTH: usize = 1024;
/// Maximum number of usage counters retained for one invocation.
const MAX_USAGE_COUNTERS: usize = 32;
/// Maximum portable name length for an identifier or usage counter.
const MAX_OUTPUT_NAME_LENGTH: usize = 128;
/// Maximum encoded size of the arbitrary JSON value returned by Codex.
const MAX_STRUCTURED_RESULT_BYTES: usize = 256 * 1024;
/// Maximum encoded size of the complete successful plugin output.
const MAX_PLUGIN_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

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
        "maxLength": MAX_FINAL_MESSAGE_LENGTH
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

  if let Some(structured_result) = outputs.get("structured_result") {
    validate_encoded_size("structured_result", structured_result, MAX_STRUCTURED_RESULT_BYTES)?;
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

fn validate_encoded_size(kind: &str, value: &impl Serialize, maximum: usize) -> anyhow::Result<()> {
  let mut counter = EncodedSizeCounter::new(maximum);
  let result = serde_json::to_writer(&mut counter, value);
  if counter.exceeded {
    bail!("{kind} exceeds {maximum} encoded bytes");
  }
  result.with_context(|| format!("failed to encode {kind}"))
}

struct EncodedSizeCounter {
  remaining: usize,
  exceeded: bool,
}

impl EncodedSizeCounter {
  fn new(maximum: usize) -> Self {
    Self {
      remaining: maximum,
      exceeded: false,
    }
  }
}

impl io::Write for EncodedSizeCounter {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    if bytes.len() > self.remaining {
      self.exceeded = true;
      return Err(io::Error::other("encoded size limit exceeded"));
    }
    self.remaining -= bytes.len();
    Ok(bytes.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
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
