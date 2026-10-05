use std::collections::HashMap;

use octa_plugin::logger::REDACTION_MARKER;
use serde_json::{json, Value};

use super::*;
use crate::events::EventDecoder;

fn sanitizer() -> RunSanitizer {
  RunSanitizer::from_variables(
    &HashMap::from([
      (
        "selected".to_owned(),
        json!({
          "token": "token-overlap-long",
          "prefix": "token-overlap",
          "number": 987654321,
          "enabled": false,
          "nested": ["nested-secret"]
        }),
      ),
      ("ordinary".to_owned(), json!("visible-value")),
    ]),
    &["selected".to_owned(), "missing".to_owned()],
  )
}

fn decode_event(value: Value) -> HarnessEvent {
  let mut frame = serde_json::to_vec(&value).unwrap();
  frame.push(b'\n');
  let mut decoder = EventDecoder::new();
  decoder.next_event(&mut frame.as_slice()).unwrap().unwrap()
}

#[test]
fn recursively_removes_selected_scalars_from_values_and_object_names() {
  let sanitized = sanitizer()
    .sanitize_value(json!({
      "message-token-overlap-long": "before token-overlap-long after",
      "number": 987654321,
      "number_text": "v987654321 987654321 987654321x",
      "enabled": false,
      "enabled_text": "falsehood false false_value",
      "nested": [{"value": "nested-secret"}],
      "ordinary": "visible-value",
      "null": null
    }))
    .unwrap();

  assert_eq!(sanitized["message-*****"], "before ***** after");
  assert_eq!(sanitized["number"], REDACTION_MARKER);
  assert_eq!(
    sanitized["number_text"],
    format!("v987654321 {REDACTION_MARKER} 987654321x")
  );
  assert_eq!(sanitized["enabled"], REDACTION_MARKER);
  assert_eq!(
    sanitized["enabled_text"],
    format!("falsehood {REDACTION_MARKER} false_value")
  );
  assert_eq!(sanitized["nested"][0]["value"], REDACTION_MARKER);
  assert_eq!(sanitized["ordinary"], "visible-value");
  assert!(sanitized["null"].is_null());

  let encoded = serde_json::to_string(&sanitized).unwrap();
  for secret in ["token-overlap", "nested-secret"] {
    assert!(!encoded.contains(secret), "sanitized JSON exposed {secret}");
  }
}

#[test]
fn consumes_a_raw_event_and_retains_only_its_sanitized_tree() {
  let event = decode_event(json!({
    "type": "item.completed",
    "item": {
      "type": "agent_message",
      "text": "credential=token-overlap-long",
      "metadata": {"nested": ["nested-secret", 987654321, false]}
    }
  }));

  let event = sanitizer().sanitize_event(event).unwrap();

  assert_eq!(event.sequence(), 0);
  assert_eq!(event.event_type(), "item.completed");
  assert_eq!(event.terminal(), None);
  assert_eq!(event.value()["item"]["text"], "credential=*****");
  assert_eq!(
    event.value()["item"]["metadata"]["nested"],
    json!(["*****", "*****", "*****"])
  );
  let trace = serde_json::to_string(event.value()).unwrap();
  assert!(!trace.contains("token-overlap"));
  assert!(!trace.contains("nested-secret"));
  assert!(!trace.contains("987654321"));
}

#[test]
fn refuses_ambiguous_objects_without_echoing_the_colliding_names() {
  let error = sanitizer()
    .sanitize_value(json!({
      "token-overlap": "first",
      "token-overlap-long": "second"
    }))
    .unwrap_err();

  assert_eq!(error, SanitizationError::KeyCollision);
  assert!(!error.to_string().contains("token-overlap"));
}

#[test]
fn keeps_unselected_values_and_uses_the_shared_free_text_rules() {
  let sanitizer = sanitizer();

  assert_eq!(sanitizer.sanitize_text("visible-value"), "visible-value");
  assert_eq!(
    sanitizer.sanitize_text("token-overlap-long and token-overlap"),
    "***** and *****"
  );
  assert_eq!(
    sanitizer.sanitize_text("v987654321 987654321 falsehood false"),
    "v987654321 ***** falsehood *****"
  );

  let empty = RunSanitizer::from_variables(&HashMap::new(), &[]);
  let original = json!({"unchanged": [42, false, "text"]});
  assert_eq!(empty.sanitize_value(original.clone()).unwrap(), original);
}
