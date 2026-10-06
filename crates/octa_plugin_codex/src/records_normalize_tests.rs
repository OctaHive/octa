use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::normalize::*;
use crate::{
  events::EventDecoder,
  sanitization::{RunSanitizer, SanitizedEvent},
};

fn terminal_event(value: Value) -> SanitizedEvent {
  let mut bytes = serde_json::to_vec(&value).unwrap();
  bytes.push(b'\n');
  let mut decoder = EventDecoder::new();
  let event = decoder.next_event(&mut bytes.as_slice()).unwrap().unwrap();
  RunSanitizer::from_variables(&HashMap::new(), &[])
    .sanitize_event(event)
    .unwrap()
}

fn result_schema(value: Value) -> Map<String, Value> {
  value.as_object().cloned().unwrap()
}

#[test]
fn normalizes_completed_and_semantic_failure_terminals_without_conflating_them_with_execution_failure() {
  let completed = normalize_terminal(
    &terminal_event(json!({
      "type": "turn.completed",
      "message": "work complete",
      "thread_id": "thread-1",
      "turn_id": "turn-2",
      "usage": { "input_tokens": 3, "output_tokens": 5 }
    })),
    None,
  )
  .unwrap();
  let (outcome, payload, identifiers, usage) = completed.into_parts();
  assert_eq!(outcome, SemanticOutcome::Completed);
  assert_eq!(payload, ResultPayload::FinalMessage("work complete".to_owned()));
  assert_eq!(identifiers["thread_id"], "thread-1");
  assert_eq!(identifiers["turn_id"], "turn-2");
  assert_eq!(usage["input_tokens"], 3);
  assert_eq!(usage["output_tokens"], 5);

  let blocked = normalize_terminal(
    &terminal_event(json!({
      "type": "turn.completed",
      "outcome": "blocked",
      "message": "operator input is required"
    })),
    None,
  )
  .unwrap();
  assert_eq!(blocked.into_parts().0, SemanticOutcome::Blocked);

  let failed = normalize_terminal(
    &terminal_event(json!({
      "type": "turn.failed",
      "error": { "message": "fixture failure" }
    })),
    None,
  )
  .unwrap();
  let (outcome, payload, _, _) = failed.into_parts();
  assert_eq!(outcome, SemanticOutcome::Failed);
  assert_eq!(payload, ResultPayload::FinalMessage("fixture failure".to_owned()));
}

#[test]
fn validates_structured_terminal_results_against_the_declared_schema() {
  let schema = result_schema(json!({
    "type": "object",
    "additionalProperties": false,
    "properties": {
      "summary": { "type": "string" },
      "issues": { "type": "integer", "minimum": 0 }
    },
    "required": ["summary", "issues"]
  }));
  let valid = normalize_terminal(
    &terminal_event(json!({
      "type": "turn.completed",
      "result": { "summary": "review complete", "issues": 0 }
    })),
    Some(&schema),
  )
  .unwrap();
  let (_, payload, _, _) = valid.into_parts();
  assert_eq!(
    payload,
    ResultPayload::Structured(json!({ "summary": "review complete", "issues": 0 }))
  );

  for terminal in [
    json!({ "type": "turn.completed", "message": "missing structured result" }),
    json!({
      "type": "turn.completed",
      "result": { "summary": "review complete", "issues": -1 }
    }),
  ] {
    let error = normalize_terminal(&terminal_event(terminal), Some(&schema)).unwrap_err();
    assert!(
      error.to_string().contains("structured result"),
      "unexpected error: {error}"
    );
  }
}

#[test]
fn rejects_missing_or_inconsistent_terminal_contract_data_with_bounded_errors() {
  let cases = [
    json!({ "type": "turn.completed" }),
    json!({ "type": "turn.completed", "outcome": "unknown", "message": "done" }),
    json!({ "type": "turn.completed", "outcome": 7, "message": "done" }),
    json!({ "type": "turn.failed", "outcome": "completed", "error": { "message": "failed" } }),
    json!({ "type": "turn.failed", "error": {} }),
    json!({ "type": "turn.completed", "message": 7 }),
    json!({ "type": "turn.completed", "message": "x".repeat(super::limits::MAX_FINAL_MESSAGE_BYTES + 1) }),
    json!({ "type": "turn.completed", "message": "done", "thread_id": 7 }),
    json!({ "type": "turn.completed", "message": "done", "usage": { "input_tokens": -1 } }),
    json!({ "type": "turn.completed", "message": "done", "usage": { "BadCounter": 1 } }),
  ];

  for terminal in cases {
    let error = normalize_terminal(&terminal_event(terminal), None).unwrap_err();
    assert!(error.to_string().len() < 192, "unbounded error: {error}");
  }

  let too_many_counters = (0..=super::MAX_USAGE_COUNTERS)
    .map(|index| (format!("counter_{index}"), json!(index)))
    .collect::<Map<_, _>>();
  let error = normalize_terminal(
    &terminal_event(json!({
      "type": "turn.completed",
      "message": "done",
      "usage": too_many_counters
    })),
    None,
  )
  .unwrap_err();
  assert!(error.to_string().contains("too many counters"));
}
