//! Translation from supported Codex activity to existing Octa responses.
//!
//! This module recognizes only the event and item types covered by the pinned
//! Codex compatibility set. Unknown types remain additive and produce no live
//! response. The type boundary accepts only [`SanitizedEvent`], so copied
//! harness values are already safe to forward or retain.

use std::{error::Error, fmt};

use octa_plugin::protocol::{DiagnosticLevel, PluginResponse, ProgressUpdate, SourceLocation};
use serde_json::{Map, Value};

use crate::sanitization::SanitizedEvent;

/// Maximum UTF-8 bytes copied into one live output or diagnostic response.
///
/// The plugin protocol permits one MiB frames. Keeping individual values at
/// 64 KiB leaves deterministic headroom for JSON escaping, response metadata,
/// command identifiers, and future additive protocol fields.
const MAX_LIVE_TEXT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes retained in an optional diagnostic file location.
const MAX_SOURCE_LOCATION_BYTES: usize = 4 * 1024;
const TRUNCATION_MARKER: &str = "... [truncated]";

/// A supported event had no safe, usable activity payload.
///
/// Diagnostics identify only the local sequence and a static contract reason;
/// untrusted harness contents are never copied into protocol-error messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventNormalizationError {
  sequence: u64,
  reason: &'static str,
}

impl fmt::Display for EventNormalizationError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      formatter,
      "Codex event {} cannot be normalized: {}",
      self.sequence, self.reason
    )
  }
}

impl Error for EventNormalizationError {}

/// Translates one decoded event into existing Octa protocol responses.
///
/// Unknown top-level and item event types are additive and therefore produce
/// no response. Known activity keeps its input order, and a single harness
/// event may yield output followed by a diagnostic.
pub(crate) fn normalize_activity(
  command_id: &str,
  event: &SanitizedEvent,
) -> Result<Vec<PluginResponse>, EventNormalizationError> {
  match event.event_type() {
    "thread.started" => Ok(vec![progress(command_id, "Codex thread started")]),
    "turn.started" => Ok(vec![progress(command_id, "Codex turn started")]),
    "item.started" => normalize_started_item(command_id, event),
    "item.completed" => normalize_completed_item(command_id, event),
    "error" => Ok(vec![diagnostic(
      command_id,
      required_string(event, event_object(event), "message", "error.message must be a string")?,
      location(event.value()),
    )]),
    "turn.failed" => {
      let error = required_object(event, event.value(), "error", "turn.failed.error must be an object")?;
      Ok(vec![diagnostic(
        command_id,
        required_string(event, error, "message", "turn.failed.error.message must be a string")?,
        error.get("location").and_then(parse_location),
      )])
    },
    _ => Ok(Vec::new()),
  }
}

fn normalize_started_item(
  command_id: &str,
  event: &SanitizedEvent,
) -> Result<Vec<PluginResponse>, EventNormalizationError> {
  let item = event_item(event)?;
  let message = match item_type(event, item)? {
    "reasoning" => Some("Codex is reasoning"),
    "command_execution" => Some("Codex is running a command"),
    "file_change" => Some("Codex is applying file changes"),
    "mcp_tool_call" => Some("Codex is calling an MCP tool"),
    "web_search" => Some("Codex is searching the web"),
    "plan_update" | "todo_list" => Some("Codex updated its plan"),
    _ => None,
  };
  Ok(
    message
      .into_iter()
      .map(|message| progress(command_id, message))
      .collect(),
  )
}

fn normalize_completed_item(
  command_id: &str,
  event: &SanitizedEvent,
) -> Result<Vec<PluginResponse>, EventNormalizationError> {
  let item = event_item(event)?;
  match item_type(event, item)? {
    "agent_message" => Ok(output_responses(
      command_id,
      required_string(event, item, "text", "agent_message.text must be a string")?,
      false,
    )),
    "command_execution" => normalize_completed_command(command_id, event, item),
    "file_change" => Ok(vec![progress(command_id, "Codex finished applying file changes")]),
    "mcp_tool_call" => normalize_completed_mcp_call(command_id, event, item),
    "web_search" => Ok(vec![progress(command_id, "Codex finished searching the web")]),
    "plan_update" | "todo_list" => Ok(vec![progress(command_id, "Codex updated its plan")]),
    "reasoning" => Ok(Vec::new()),
    _ => Ok(Vec::new()),
  }
}

fn normalize_completed_command(
  command_id: &str,
  event: &SanitizedEvent,
  item: &Map<String, Value>,
) -> Result<Vec<PluginResponse>, EventNormalizationError> {
  let output = required_string(
    event,
    item,
    "aggregated_output",
    "command_execution.aggregated_output must be a string",
  )?;
  let exit_code = optional_i64(
    event,
    item,
    "exit_code",
    "command_execution.exit_code must be an integer",
  )?;
  let status = optional_string(event, item, "status", "command_execution.status must be a string")?;
  let failed = exit_code.is_some_and(|code| code != 0) || status == Some("failed");
  let mut responses = output_responses(command_id, output, failed);
  if failed {
    let message = match exit_code {
      Some(code) => format!("Codex command failed with exit code {code}"),
      None => "Codex command failed".to_owned(),
    };
    responses.push(diagnostic(command_id, &message, None));
  }
  Ok(responses)
}

fn normalize_completed_mcp_call(
  command_id: &str,
  event: &SanitizedEvent,
  item: &Map<String, Value>,
) -> Result<Vec<PluginResponse>, EventNormalizationError> {
  let Some(error) = item.get("error").filter(|error| !error.is_null()) else {
    return Ok(vec![progress(command_id, "Codex finished an MCP tool call")]);
  };
  let message = error
    .as_str()
    .or_else(|| error.as_object()?.get("message")?.as_str())
    .ok_or_else(|| normalization_error(event, "mcp_tool_call.error must contain a string message"))?;
  Ok(vec![diagnostic(
    command_id,
    message,
    error.get("location").and_then(parse_location),
  )])
}

fn event_item(event: &SanitizedEvent) -> Result<&Map<String, Value>, EventNormalizationError> {
  required_object(event, event.value(), "item", "item must be an object")
}

fn event_object(event: &SanitizedEvent) -> &Map<String, Value> {
  event
    .value()
    .as_object()
    .expect("decoded harness events always contain JSON objects")
}

fn item_type<'a>(event: &SanitizedEvent, item: &'a Map<String, Value>) -> Result<&'a str, EventNormalizationError> {
  item
    .get("type")
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty())
    .ok_or_else(|| normalization_error(event, "item.type must be a non-empty string"))
}

fn required_object<'a>(
  event: &SanitizedEvent,
  value: &'a Value,
  field: &str,
  reason: &'static str,
) -> Result<&'a Map<String, Value>, EventNormalizationError> {
  value
    .as_object()
    .and_then(|object| object.get(field))
    .and_then(Value::as_object)
    .ok_or_else(|| normalization_error(event, reason))
}

fn required_string<'a>(
  event: &SanitizedEvent,
  object: &'a Map<String, Value>,
  field: &str,
  reason: &'static str,
) -> Result<&'a str, EventNormalizationError> {
  object
    .get(field)
    .and_then(Value::as_str)
    .ok_or_else(|| normalization_error(event, reason))
}

fn optional_i64(
  event: &SanitizedEvent,
  object: &Map<String, Value>,
  field: &str,
  reason: &'static str,
) -> Result<Option<i64>, EventNormalizationError> {
  match object.get(field) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => value
      .as_i64()
      .map(Some)
      .ok_or_else(|| normalization_error(event, reason)),
  }
}

fn optional_string<'a>(
  event: &SanitizedEvent,
  object: &'a Map<String, Value>,
  field: &str,
  reason: &'static str,
) -> Result<Option<&'a str>, EventNormalizationError> {
  match object.get(field) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => value
      .as_str()
      .map(Some)
      .ok_or_else(|| normalization_error(event, reason)),
  }
}

fn normalization_error(event: &SanitizedEvent, reason: &'static str) -> EventNormalizationError {
  EventNormalizationError {
    sequence: event.sequence(),
    reason,
  }
}

fn progress(command_id: &str, message: &str) -> PluginResponse {
  PluginResponse::Progress {
    id: command_id.to_owned(),
    progress: ProgressUpdate {
      message: message.to_owned(),
      current: None,
      total: None,
      unit: None,
    },
  }
}

fn output_responses(command_id: &str, text: &str, stderr: bool) -> Vec<PluginResponse> {
  let mut remaining = text;
  let mut responses = Vec::new();
  while !remaining.is_empty() {
    let end = utf8_prefix(remaining, MAX_LIVE_TEXT_BYTES);
    let line = remaining[..end].to_owned();
    responses.push(if stderr {
      PluginResponse::Stderr {
        id: command_id.to_owned(),
        line,
      }
    } else {
      PluginResponse::Stdout {
        id: command_id.to_owned(),
        line,
      }
    });
    remaining = &remaining[end..];
  }
  responses
}

fn diagnostic(command_id: &str, message: &str, location: Option<SourceLocation>) -> PluginResponse {
  PluginResponse::Diagnostic {
    id: command_id.to_owned(),
    level: DiagnosticLevel::Error,
    message: bounded_text(message),
    location,
  }
}

fn bounded_text(value: &str) -> String {
  if value.len() <= MAX_LIVE_TEXT_BYTES {
    return value.to_owned();
  }
  let maximum_prefix = MAX_LIVE_TEXT_BYTES - TRUNCATION_MARKER.len();
  let end = utf8_prefix(value, maximum_prefix);
  format!("{}{TRUNCATION_MARKER}", &value[..end])
}

fn utf8_prefix(value: &str, maximum: usize) -> usize {
  if value.len() <= maximum {
    return value.len();
  }
  let mut end = maximum;
  while !value.is_char_boundary(end) {
    end -= 1;
  }
  end
}

fn location(value: &Value) -> Option<SourceLocation> {
  value.as_object()?.get("location").and_then(parse_location)
}

fn parse_location(value: &Value) -> Option<SourceLocation> {
  let object = value.as_object()?;
  let file = object.get("file")?.as_str()?;
  if file.is_empty() || file.len() > MAX_SOURCE_LOCATION_BYTES || file.chars().any(char::is_control) {
    return None;
  }
  Some(SourceLocation {
    file: file.to_owned(),
    line: object.get("line").and_then(Value::as_u64),
    column: object.get("column").and_then(Value::as_u64),
  })
}

#[cfg(test)]
#[path = "normalize_tests.rs"]
mod tests;
