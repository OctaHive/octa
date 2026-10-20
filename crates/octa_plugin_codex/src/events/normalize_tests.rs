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

  let success_without_status = normalize_json(json!({
    "type": "item.completed",
    "item": {"type": "command_execution", "aggregated_output": "ok", "exit_code": 0}
  }))
  .unwrap();
  assert!(matches!(
    success_without_status.as_slice(),
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

#[test]
fn public_command_observation_retains_outcome_without_private_invocation() {
  let responses = normalize_json(json!({
    "type":"item.completed", "item":{
      "id":"command-private-id", "type":"command_execution", "status":"failed", "exit_code":17,
      "command":"sh -c private-argument", "aggregated_output":"protected-output",
      "prompt":"private-prompt", "environment":{"TOKEN":"private-credential"}
    }
  }))
  .unwrap();
  let observations = responses
    .iter()
    .map(|r| serde_json::to_value(r).unwrap())
    .filter_map(|r| r["payload"]["progress"]["observation"].as_object().cloned())
    .collect::<Vec<_>>();
  assert_eq!(
    observations.len(),
    1,
    "one structured operation, independently of progress/output"
  );
  let observation = &observations[0];
  assert_eq!(observation["phase"], "completed");
  assert_eq!(observation["operation"]["kind"], "command");
  assert_eq!(observation["operation"]["exit_code"], 17);
  assert_eq!(observation["operation"]["failed"], true);
  assert!(observation["observed_at_unix_ms"].as_i64().unwrap() > 0);
  let encoded = serde_json::to_string(observation).unwrap();
  for private in [
    "command-private-id",
    "private-argument",
    "protected-output",
    "private-prompt",
    "private-credential",
  ] {
    assert!(!encoded.contains(private));
  }
}

#[test]
fn public_file_observations_keep_only_bounded_workspace_paths_and_reported_boundaries() {
  let mut observed = Vec::new();
  for phase in ["item.started", "item.completed"] {
    for response in normalize_json(json!({"type":phase, "item": {
      "id":"edit-1", "type":"file_change", "status":"completed", "changes":[
        {"path":"src/main.rs", "kind":"update", "diff":"protected-file-content"},
        {"path":"/workspace/protected/test.rs", "kind":"update"},
        {"path":"../protected/test.rs", "kind":"delete"},
        {"path":".octa/private.json", "kind":"add"},
        {"path":"src/../../private", "kind":"update"},
        {"path":"src\\private", "kind":"update"}
      ]
    }}))
    .unwrap()
    {
      let value = serde_json::to_value(response).unwrap();
      if value["payload"]["progress"]["observation"].is_object() {
        observed.push(value["payload"]["progress"]["observation"].clone());
      }
    }
  }
  assert_eq!(observed.len(), 2);
  assert_eq!(observed[0]["phase"], "started");
  assert_eq!(observed[1]["phase"], "completed");
  assert_eq!(observed[0]["operation_id"], observed[1]["operation_id"]);
  assert_eq!(
    observed[1]["operation"],
    json!({"kind":"file", "path":"src/main.rs", "change":"modified", "failed":false})
  );
  let serialized = serde_json::to_string(&observed).unwrap();
  assert!(!serialized.contains("protected"));
  assert!(!serialized.contains("private"));
}

#[test]
fn public_command_completion_without_measurements_keeps_the_outcome_unknown() {
  let responses = normalize_json(json!({
    "type":"item.completed", "item":{
      "id":"unmeasured-command", "type":"command_execution", "aggregated_output":""
    }
  }))
  .unwrap();
  let observations = responses
    .iter()
    .filter_map(|response| match response {
      PluginResponse::Progress { progress, .. } => progress.observation.as_ref(),
      _ => None,
    })
    .collect::<Vec<_>>();
  assert_eq!(observations.len(), 1);
  assert_eq!(observations[0].phase, octa_runner_protocol::OperationPhase::Completed);
  assert_eq!(
    observations[0].operation,
    octa_runner_protocol::ObservedOperation::Command {
      exit_code: None,
      failed: None
    }
  );
}

#[test]
fn public_file_completion_preserves_creation_and_deletion_in_reported_order() {
  let responses = normalize_json(json!({"type":"item.completed", "item": {
    "id":"edit", "type":"file_change", "status":"completed", "changes":[
      {"path":"src/new.rs", "kind":"add"},
      {"path":"src/old.rs", "kind":"delete"},
      {"path":"src/unknown.rs", "kind":"future-change"}
    ]
  }}))
  .unwrap();
  let operations = responses
    .iter()
    .filter_map(|response| match response {
      PluginResponse::Progress { progress, .. } => progress.observation.as_ref().map(|v| v.operation.clone()),
      _ => None,
    })
    .collect::<Vec<_>>();
  use octa_runner_protocol::{FileChangeKind, ObservedOperation};
  assert_eq!(
    operations,
    vec![
      ObservedOperation::File {
        path: "src/new.rs".to_owned(),
        change: FileChangeKind::Created,
        failed: Some(false)
      },
      ObservedOperation::File {
        path: "src/old.rs".to_owned(),
        change: FileChangeKind::Deleted,
        failed: Some(false)
      },
    ]
  );
}

#[test]
fn public_file_observations_require_a_bounded_reported_changes_list() {
  let changes = |count| {
    Value::Array(
      (0..count)
        .map(|n| json!({"path":format!("out/{n}.txt"),"kind":"add"}))
        .collect(),
    )
  };
  for (reported, expected) in [
    (None, 0),
    (Some(Value::Null), 0),
    (Some(json!("unstructured")), 0),
    (Some(changes(32)), 32),
    (Some(changes(33)), 0),
  ] {
    let mut event = json!({"type":"item.completed", "item":{"id":"edit", "type":"file_change"}});
    if let Some(reported) = reported {
      event["item"]["changes"] = reported;
    }
    let responses = normalize_json(event).unwrap();
    let observations = responses
      .iter()
      .filter_map(|response| match response {
        PluginResponse::Progress { progress, .. } => progress.observation.as_ref(),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(observations.len(), expected);
    assert!(observations.iter().all(|v| v.is_valid()));
    assert!(matches!(&responses[0], PluginResponse::Progress { progress, .. }
      if progress.message == "Codex finished applying file changes" && progress.observation.is_none()));
  }
}
