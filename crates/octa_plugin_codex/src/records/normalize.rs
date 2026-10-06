//! Normalization of sanitized Codex terminal events.
//!
//! This module converts the pinned harness terminal contract into one typed
//! semantic result. It does not write records or emit plugin responses; those
//! side effects happen only after this validation boundary succeeds.

use std::collections::BTreeMap;

use anyhow::bail;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use super::{
  limits::{validate_encoded_size, MAX_FINAL_MESSAGE_BYTES, MAX_STRUCTURED_RESULT_BYTES, MAX_USAGE_METADATA_BYTES},
  MAX_HARNESS_IDENTIFIER_LENGTH, MAX_OUTPUT_NAME_LENGTH, MAX_USAGE_COUNTERS,
};
use crate::{events::TerminalEvent, sanitization::SanitizedEvent};

/// Semantic conclusion reported by a well-formed Codex terminal event.
///
/// These values do not describe plugin execution success. Every variant is an
/// auditable result and therefore remains eligible for a code-zero plugin
/// completion once its run records have been committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SemanticOutcome {
  Completed,
  Blocked,
  NeedsInput,
  BudgetExhausted,
  Failed,
}

impl SemanticOutcome {
  /// Stable outcomes shared by parsing, serialization, and the public schema.
  const VALUES: [(Self, &'static str); 5] = [
    (Self::Completed, "completed"),
    (Self::Blocked, "blocked"),
    (Self::NeedsInput, "needs_input"),
    (Self::BudgetExhausted, "budget_exhausted"),
    (Self::Failed, "failed"),
  ];

  fn parse(value: &str) -> Option<Self> {
    Self::VALUES
      .iter()
      .find_map(|(outcome, name)| (*name == value).then_some(*outcome))
  }

  /// Stable string used by plugin outputs and versioned record documents.
  pub(crate) fn as_str(self) -> &'static str {
    Self::VALUES
      .iter()
      .find_map(|(outcome, name)| (*outcome == self).then_some(*name))
      .expect("every semantic outcome has one stable name")
  }

  /// Names advertised by the plugin's public output schema.
  pub(crate) fn names() -> [&'static str; 5] {
    Self::VALUES.map(|(_, name)| name)
  }
}

impl Serialize for SemanticOutcome {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(self.as_str())
  }
}

/// Exactly one terminal payload retained for a normalized result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ResultPayload {
  /// Bounded terminal text used when no result schema was configured.
  FinalMessage(String),
  /// Sanitized JSON value accepted by the configured result schema.
  Structured(Value),
}

/// Sanitized, schema-validated evidence from one terminal harness event.
///
/// Run-record paths are intentionally absent: task 6.2 durably commits those
/// files before this value can cross the plugin protocol boundary.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NormalizedResult {
  outcome: SemanticOutcome,
  payload: ResultPayload,
  harness_identifiers: BTreeMap<String, String>,
  usage: BTreeMap<String, u64>,
}

impl NormalizedResult {
  /// Transfers normalized data to the run-record writer without reparsing the
  /// untrusted terminal event.
  pub(crate) fn into_parts(
    self,
  ) -> (
    SemanticOutcome,
    ResultPayload,
    BTreeMap<String, String>,
    BTreeMap<String, u64>,
  ) {
    (self.outcome, self.payload, self.harness_identifiers, self.usage)
  }
}

/// Normalizes one already-sanitized terminal event.
///
/// A `turn.failed` event is a semantic result, not a process failure. Missing
/// payloads, contradictory terminal states, malformed metadata, and structured
/// values rejected by the declared schema are execution-contract failures.
/// Error messages remain static so rejected harness data cannot reach logs.
pub(crate) fn normalize_terminal(
  event: &SanitizedEvent,
  result_schema: Option<&Map<String, Value>>,
) -> anyhow::Result<NormalizedResult> {
  let terminal = event
    .terminal()
    .ok_or_else(|| anyhow::anyhow!("Codex result normalization requires a terminal event"))?;
  let object = event
    .value()
    .as_object()
    .expect("decoded harness events always contain JSON objects");
  let outcome = normalize_outcome(object, terminal)?;
  let payload = match result_schema {
    Some(schema) => normalize_structured_result(object, schema)?,
    None => ResultPayload::FinalMessage(normalize_final_message(object, terminal)?),
  };
  let harness_identifiers = normalize_identifiers(object)?;
  let usage = normalize_usage(object)?;

  Ok(NormalizedResult {
    outcome,
    payload,
    harness_identifiers,
    usage,
  })
}

fn normalize_outcome(object: &Map<String, Value>, terminal: TerminalEvent) -> anyhow::Result<SemanticOutcome> {
  let explicit = match object.get("outcome") {
    None => None,
    Some(Value::String(value)) => {
      Some(SemanticOutcome::parse(value).ok_or_else(|| anyhow::anyhow!("Codex terminal outcome is not supported"))?)
    },
    Some(_) => bail!("Codex terminal outcome must be a string"),
  };
  match (terminal, explicit) {
    (TerminalEvent::Completed, outcome) => Ok(outcome.unwrap_or(SemanticOutcome::Completed)),
    (TerminalEvent::Failed, None | Some(SemanticOutcome::Failed)) => Ok(SemanticOutcome::Failed),
    (TerminalEvent::Failed, Some(_)) => bail!("Codex failed terminal event has a contradictory outcome"),
  }
}

fn normalize_final_message(object: &Map<String, Value>, terminal: TerminalEvent) -> anyhow::Result<String> {
  let message = match object.get("message") {
    Some(Value::String(message)) => Some(message.as_str()),
    Some(_) => bail!("Codex terminal message must be a string"),
    None if terminal == TerminalEvent::Failed => object
      .get("error")
      .and_then(Value::as_object)
      .and_then(|error| error.get("message"))
      .and_then(Value::as_str),
    None => None,
  }
  .filter(|message| !message.is_empty())
  .ok_or_else(|| anyhow::anyhow!("Codex terminal event is missing a non-empty final message"))?;
  if message.len() > MAX_FINAL_MESSAGE_BYTES {
    bail!("final message exceeds {MAX_FINAL_MESSAGE_BYTES} UTF-8 bytes");
  }
  Ok(message.to_owned())
}

fn normalize_structured_result(
  object: &Map<String, Value>,
  schema: &Map<String, Value>,
) -> anyhow::Result<ResultPayload> {
  let result = object
    .get("result")
    .ok_or_else(|| anyhow::anyhow!("Codex terminal event is missing the configured structured result"))?;
  validate_encoded_size("structured_result", result, MAX_STRUCTURED_RESULT_BYTES)?;
  let validator = jsonschema::validator_for(&Value::Object(schema.clone()))
    .map_err(|_| anyhow::anyhow!("configured result schema could not be compiled"))?;
  if !validator.is_valid(result) {
    bail!("Codex structured result does not satisfy the configured schema");
  }
  Ok(ResultPayload::Structured(result.clone()))
}

fn normalize_identifiers(object: &Map<String, Value>) -> anyhow::Result<BTreeMap<String, String>> {
  let mut identifiers = BTreeMap::new();
  for name in ["thread_id", "turn_id"] {
    let Some(value) = object.get(name) else {
      continue;
    };
    let value = value
      .as_str()
      .filter(|value| {
        !value.is_empty() && value.len() <= MAX_HARNESS_IDENTIFIER_LENGTH && !value.chars().any(char::is_control)
      })
      .ok_or_else(|| anyhow::anyhow!("Codex terminal harness identifier is invalid"))?;
    identifiers.insert(name.to_owned(), value.to_owned());
  }
  Ok(identifiers)
}

fn normalize_usage(object: &Map<String, Value>) -> anyhow::Result<BTreeMap<String, u64>> {
  let Some(value) = object.get("usage") else {
    return Ok(BTreeMap::new());
  };
  let usage = value
    .as_object()
    .ok_or_else(|| anyhow::anyhow!("Codex terminal usage metadata must be an object"))?;
  if usage.len() > MAX_USAGE_COUNTERS {
    bail!("Codex terminal usage metadata has too many counters");
  }
  let mut normalized = BTreeMap::new();
  for (name, value) in usage {
    if !valid_output_name(name) {
      bail!("Codex terminal usage counter name is invalid");
    }
    let value = value
      .as_u64()
      .ok_or_else(|| anyhow::anyhow!("Codex terminal usage counter must be a non-negative integer"))?;
    normalized.insert(name.clone(), value);
  }
  validate_encoded_size("usage metadata", &normalized, MAX_USAGE_METADATA_BYTES)?;
  Ok(normalized)
}

fn valid_output_name(value: &str) -> bool {
  let mut bytes = value.bytes();
  value.len() <= MAX_OUTPUT_NAME_LENGTH
    && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
    && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}
