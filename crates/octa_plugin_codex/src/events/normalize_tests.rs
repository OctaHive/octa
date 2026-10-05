use octa_plugin::protocol::{DiagnosticLevel, PluginResponse};
use serde_json::{json, Value};

use super::*;
use crate::{
  events::{EventDecoder, HarnessEvent},
  sanitization::{RunSanitizer, SanitizedEvent},
};

fn decode_event(frame: &[u8]) -> HarnessEvent {
  let mut decoder = EventDecoder::new();
  let mut input = frame;
  let event = decoder.next_event(&mut input).unwrap();
  assert!(input.is_empty());
  event.expect("fixture frame must contain one event")
}

fn normalize_json(value: Value) -> Result<Vec<PluginResponse>, EventNormalizationError> {
  let mut frame = serde_json::to_vec(&value).unwrap();
  frame.push(b'\n');
  let sanitizer = RunSanitizer::from_variables(&Default::default(), &[]);
  let event: SanitizedEvent = sanitizer.sanitize_event(decode_event(&frame)).unwrap();
  normalize_activity("command", &event)
}

#[test]
fn normalizes_supported_started_activity_to_progress() {
  let cases = [
    (json!({"type": "thread.started"}), "Codex thread started"),
    (json!({"type": "turn.started"}), "Codex turn started"),
    (
      json!({"type": "item.started", "item": {"type": "reasoning"}}),
      "Codex is reasoning",
    ),
    (
      json!({"type": "item.started", "item": {"type": "command_execution"}}),
      "Codex is running a command",
    ),
    (
      json!({"type": "item.started", "item": {"type": "file_change"}}),
      "Codex is applying file changes",
    ),
    (
      json!({"type": "item.started", "item": {"type": "mcp_tool_call"}}),
      "Codex is calling an MCP tool",
    ),
    (
      json!({"type": "item.started", "item": {"type": "web_search"}}),
      "Codex is searching the web",
    ),
    (
      json!({"type": "item.started", "item": {"type": "plan_update"}}),
      "Codex updated its plan",
    ),
    (
      json!({"type": "item.started", "item": {"type": "todo_list"}}),
      "Codex updated its plan",
    ),
  ];

  for (value, expected) in cases {
    let responses = normalize_json(value).unwrap();
    let [PluginResponse::Progress { id, progress }] = responses.as_slice() else {
      panic!("expected one progress response");
    };
    assert_eq!(id, "command");
    assert_eq!(progress.message, expected);
    assert_eq!(progress.current, None);
    assert_eq!(progress.total, None);
    assert_eq!(progress.unit, None);
  }
}

#[test]
fn normalizes_messages_and_command_results_to_ordered_output() {
  let message = normalize_json(json!({
    "type": "item.completed",
    "item": {"type": "agent_message", "text": "done"}
  }))
  .unwrap();
  let [PluginResponse::Stdout { id, line }] = message.as_slice() else {
    panic!("expected agent stdout");
  };
  assert_eq!(id, "command");
  assert_eq!(line, "done");

  let success = normalize_json(json!({
    "type": "item.completed",
    "item": {
      "type": "command_execution",
      "aggregated_output": "ok",
      "exit_code": 0,
      "status": "completed"
    }
  }))
  .unwrap();
  assert!(matches!(
    success.as_slice(),
    [PluginResponse::Stdout { line, .. }] if line == "ok"
  ));

  let failure = normalize_json(json!({
    "type": "item.completed",
    "item": {
      "type": "command_execution",
      "aggregated_output": "failed output",
      "exit_code": 7,
      "status": "failed"
    }
  }))
  .unwrap();
  assert!(matches!(
    failure.as_slice(),
    [
      PluginResponse::Stderr { line, .. },
      PluginResponse::Diagnostic {
        level: DiagnosticLevel::Error,
        message,
        location: None,
        ..
      }
    ] if line == "failed output" && message == "Codex command failed with exit code 7"
  ));

  let status_only_failure = normalize_json(json!({
    "type": "item.completed",
    "item": {
      "type": "command_execution",
      "aggregated_output": "",
      "status": "failed"
    }
  }))
  .unwrap();
  assert!(matches!(
    status_only_failure.as_slice(),
    [PluginResponse::Diagnostic { message, .. }] if message == "Codex command failed"
  ));
}

#[test]
fn normalizes_failures_with_safe_optional_source_locations() {
  let error = normalize_json(json!({
    "type": "error",
    "message": "cannot edit",
    "location": {"file": "src/lib.rs", "line": 12, "column": 3}
  }))
  .unwrap();
  let [PluginResponse::Diagnostic {
    id,
    level: DiagnosticLevel::Error,
    message,
    location: Some(location),
  }] = error.as_slice()
  else {
    panic!("expected one located diagnostic");
  };
  assert_eq!(id, "command");
  assert_eq!(message, "cannot edit");
  assert_eq!(location.file, "src/lib.rs");
  assert_eq!(location.line, Some(12));
  assert_eq!(location.column, Some(3));

  let failed = normalize_json(json!({
    "type": "turn.failed",
    "error": {"message": "budget exhausted", "location": {"file": "prompt.md"}}
  }))
  .unwrap();
  assert!(matches!(
    failed.as_slice(),
    [PluginResponse::Diagnostic { message, location: Some(location), .. }]
      if message == "budget exhausted" && location.file == "prompt.md"
  ));

  let unsafe_location = normalize_json(json!({
    "type": "error",
    "message": "bad location",
    "location": {"file": "bad\npath", "line": 1}
  }))
  .unwrap();
  assert!(matches!(
    unsafe_location.as_slice(),
    [PluginResponse::Diagnostic { location: None, .. }]
  ));
}

#[test]
fn ignores_additive_activity_and_rejects_malformed_supported_events() {
  for value in [
    json!({"type": "future.event", "secret": "opaque"}),
    json!({"type": "item.started", "item": {"type": "future_item"}}),
    json!({"type": "item.completed", "item": {"type": "reasoning"}}),
    json!({"type": "item.completed", "item": {"type": "future_item"}}),
    json!({"type": "turn.completed", "usage": {}}),
  ] {
    assert!(normalize_json(value).unwrap().is_empty());
  }

  for (value, reason) in [
    (json!({"type": "item.started"}), "item must be an object"),
    (
      json!({"type": "item.started", "item": {"type": ""}}),
      "item.type must be a non-empty string",
    ),
    (
      json!({"type": "item.completed", "item": {"type": "agent_message"}}),
      "agent_message.text must be a string",
    ),
    (
      json!({"type": "item.completed", "item": {"type": "command_execution"}}),
      "command_execution.aggregated_output must be a string",
    ),
    (
      json!({
        "type": "item.completed",
        "item": {"type": "command_execution", "aggregated_output": "", "exit_code": "zero"}
      }),
      "command_execution.exit_code must be an integer",
    ),
    (
      json!({
        "type": "item.completed",
        "item": {"type": "command_execution", "aggregated_output": "", "status": 7}
      }),
      "command_execution.status must be a string",
    ),
    (
      json!({"type": "error", "message": {"secret": "must-not-appear"}}),
      "error.message must be a string",
    ),
    (
      json!({"type": "turn.failed", "error": "bad"}),
      "turn.failed.error must be an object",
    ),
    (
      json!({"type": "turn.failed", "error": {}}),
      "turn.failed.error.message must be a string",
    ),
  ] {
    let error = normalize_json(value).unwrap_err();
    assert_eq!(error.reason, reason);
    assert!(error.to_string().len() < 160);
    assert!(!error.to_string().contains("must-not-appear"));
  }
}

#[test]
fn bounds_live_messages_and_preserves_unicode_when_chunking() {
  let text = format!("{}€tail", "x".repeat(MAX_LIVE_TEXT_BYTES - 1));
  let responses = normalize_json(json!({
    "type": "item.completed",
    "item": {"type": "agent_message", "text": text}
  }))
  .unwrap();
  assert_eq!(responses.len(), 2);
  let reconstructed = responses
    .iter()
    .map(|response| match response {
      PluginResponse::Stdout { line, .. } => line.as_str(),
      _ => panic!("expected bounded stdout chunks"),
    })
    .collect::<String>();
  assert_eq!(reconstructed, text);
  assert!(responses.iter().all(|response| match response {
    PluginResponse::Stdout { line, .. } => line.len() <= MAX_LIVE_TEXT_BYTES,
    _ => false,
  }));

  let diagnostics = normalize_json(json!({
    "type": "error",
    "message": "x".repeat(MAX_LIVE_TEXT_BYTES + 1),
    "location": {"file": "x".repeat(MAX_SOURCE_LOCATION_BYTES + 1)}
  }))
  .unwrap();
  assert!(matches!(
    diagnostics.as_slice(),
    [PluginResponse::Diagnostic { message, location: None, .. }]
      if message.len() <= MAX_LIVE_TEXT_BYTES && message.ends_with(TRUNCATION_MARKER)
  ));
}

#[test]
fn completed_non_output_items_report_progress_or_diagnostics() {
  let cases = [
    (
      json!({"type": "item.completed", "item": {"type": "file_change"}}),
      "Codex finished applying file changes",
    ),
    (
      json!({"type": "item.completed", "item": {"type": "web_search"}}),
      "Codex finished searching the web",
    ),
    (
      json!({"type": "item.completed", "item": {"type": "mcp_tool_call"}}),
      "Codex finished an MCP tool call",
    ),
    (
      json!({"type": "item.completed", "item": {"type": "mcp_tool_call", "error": null}}),
      "Codex finished an MCP tool call",
    ),
    (
      json!({"type": "item.completed", "item": {"type": "plan_update"}}),
      "Codex updated its plan",
    ),
    (
      json!({"type": "item.completed", "item": {"type": "todo_list"}}),
      "Codex updated its plan",
    ),
  ];
  for (value, expected) in cases {
    assert!(matches!(
      normalize_json(value).unwrap().as_slice(),
      [PluginResponse::Progress { progress, .. }] if progress.message == expected
    ));
  }

  for failed_call in [
    json!({
      "type": "item.completed",
      "item": {"type": "mcp_tool_call", "error": {"message": "tool failed"}}
    }),
    json!({
      "type": "item.completed",
      "item": {"type": "mcp_tool_call", "error": "tool failed"}
    }),
  ] {
    assert!(matches!(
      normalize_json(failed_call).unwrap().as_slice(),
      [PluginResponse::Diagnostic { message, .. }] if message == "tool failed"
    ));
  }

  let malformed_call = normalize_json(json!({
    "type": "item.completed",
    "item": {"type": "mcp_tool_call", "error": {"code": 7}}
  }))
  .unwrap_err();
  assert_eq!(
    malformed_call.reason,
    "mcp_tool_call.error must contain a string message"
  );
}
