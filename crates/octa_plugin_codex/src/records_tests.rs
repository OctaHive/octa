use serde_json::{json, Value};

use super::*;

fn validator() -> jsonschema::Validator {
  jsonschema::validator_for(&Value::Object(output_schema())).expect("Codex output schema must compile")
}

fn record_paths() -> Value {
  json!({
    "trace": ".octa/codex-runs/invocation/trace.jsonl",
    "result": ".octa/codex-runs/invocation/result.json",
    "provenance": ".octa/codex-runs/invocation/provenance.json"
  })
}

#[test]
fn accepts_completed_structured_and_blocked_plain_results() {
  let validator = validator();
  let completed = json!({
    "outcome": "completed",
    "structured_result": { "summary": "review complete", "issues": 0 },
    "harness_identifiers": {
      "thread_id": "thread-1",
      "turn_id": "turn-1"
    },
    "usage": {
      "input_tokens": 1200,
      "cached_input_tokens": 800,
      "output_tokens": 240
    },
    "record_paths": record_paths()
  });
  let blocked = json!({
    "outcome": "blocked",
    "final_message": "The required signing credential is unavailable.",
    "harness_identifiers": {},
    "usage": {},
    "record_paths": record_paths()
  });

  assert!(validator.is_valid(&completed));
  assert!(validator.is_valid(&blocked));
  validate_outputs(completed.as_object().unwrap()).unwrap();
  validate_outputs(blocked.as_object().unwrap()).unwrap();
}

#[test]
fn rejects_an_oversized_nested_structured_result_before_protocol_output() {
  let value = json!({
    "outcome": "completed",
    "structured_result": {
      "nested": {
        "payload": "x".repeat(MAX_STRUCTURED_RESULT_BYTES)
      }
    },
    "harness_identifiers": {},
    "usage": {},
    "record_paths": record_paths()
  });

  assert!(validator().is_valid(&value));
  let error = validate_outputs(value.as_object().unwrap()).unwrap_err();
  assert!(error.to_string().contains("structured_result exceeds"));
}

#[test]
fn production_validation_rejects_schema_invalid_outputs() {
  let value = json!({
    "outcome": "completed",
    "final_message": "done",
    "harness_identifiers": {},
    "usage": {}
  });

  let error = validate_outputs(value.as_object().unwrap()).unwrap_err();
  assert!(error.to_string().contains("advertised schema"));
}

#[test]
fn harness_results_require_exactly_one_terminal_payload_and_all_audit_fields() {
  let common = json!({
    "outcome": "completed",
    "harness_identifiers": {},
    "usage": {},
    "record_paths": record_paths()
  });

  let neither = common.clone();
  let mut both = common.clone();
  both["final_message"] = json!("done");
  both["structured_result"] = json!({ "done": true });
  let mut missing_records = common;
  missing_records["final_message"] = json!("done");
  missing_records.as_object_mut().unwrap().remove("record_paths");

  for value in [neither, both, missing_records] {
    assert!(!validator().is_valid(&value), "schema accepted {value}");
  }
}

#[test]
fn rejects_unknown_fields_malformed_counters_and_unsafe_record_paths() {
  let invalid = [
    json!({
      "outcome": "completed",
      "final_message": "done",
      "harness_identifiers": {},
      "usage": {},
      "record_paths": record_paths(),
      "unexpected": true
    }),
    json!({
      "outcome": "unknown",
      "final_message": "done",
      "harness_identifiers": {},
      "usage": {},
      "record_paths": record_paths()
    }),
    json!({
      "outcome": "completed",
      "final_message": "done",
      "harness_identifiers": { "thread_id": 7 },
      "usage": { "input_tokens": -1 },
      "record_paths": record_paths()
    }),
    json!({
      "outcome": "completed",
      "final_message": "done",
      "harness_identifiers": {},
      "usage": {},
      "record_paths": {
        "trace": "../trace.jsonl",
        "result": "result.json",
        "provenance": "provenance.json"
      }
    }),
  ];

  for value in invalid {
    assert!(!validator().is_valid(&value), "schema accepted {value}");
  }
}

#[test]
fn enforces_message_identifier_and_collection_bounds() {
  let too_many_identifiers = (0..=MAX_HARNESS_IDENTIFIERS)
    .map(|index| (format!("id_{index}"), json!(format!("value-{index}"))))
    .collect::<serde_json::Map<_, _>>();
  let too_many_counters = (0..=MAX_USAGE_COUNTERS)
    .map(|index| (format!("counter_{index}"), json!(index)))
    .collect::<serde_json::Map<_, _>>();

  for value in [
    json!({
      "outcome": "completed",
      "final_message": "x".repeat(MAX_FINAL_MESSAGE_LENGTH + 1),
      "harness_identifiers": {},
      "usage": {},
      "record_paths": record_paths()
    }),
    json!({
      "outcome": "completed",
      "final_message": "done",
      "harness_identifiers": { "thread_id": "x".repeat(MAX_HARNESS_IDENTIFIER_LENGTH + 1) },
      "usage": {},
      "record_paths": record_paths()
    }),
    json!({
      "outcome": "completed",
      "final_message": "done",
      "harness_identifiers": too_many_identifiers,
      "usage": {},
      "record_paths": record_paths()
    }),
    json!({
      "outcome": "completed",
      "final_message": "done",
      "harness_identifiers": {},
      "usage": too_many_counters,
      "record_paths": record_paths()
    }),
  ] {
    assert!(!validator().is_valid(&value), "schema accepted oversized output");
  }
}
