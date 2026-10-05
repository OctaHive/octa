//! Destructive sanitization of untrusted harness data before any output sink.
//!
//! The sanitizer consumes raw events and arbitrary owned JSON values. String
//! secrets are removed wherever they occur; numeric and boolean secrets are
//! removed when they appear as exact JSON scalars or standalone text tokens.
//! Object keys are sanitized as well because JSON names are serialized into
//! traces and diagnostics just like values.

use std::{collections::HashMap, error::Error, fmt};

use octa_plugin::logger::{collect_variable_redactions, redact, Redaction};
use serde_json::{Map, Value};

use crate::events::{HarnessEvent, TerminalEvent};

/// One event whose discriminator and complete JSON tree contain no selected
/// resolved secret scalar.
///
/// Construction remains private to this module. Downstream normalization,
/// trace serialization, and terminal-result collection therefore cannot
/// manufacture a sanitized event from an unchecked JSON value.
pub(crate) struct SanitizedEvent {
  sequence: u64,
  event_type: String,
  value: Value,
  terminal: Option<TerminalEvent>,
}

impl SanitizedEvent {
  /// Returns the zero-based position assigned by the decoder.
  pub(crate) fn sequence(&self) -> u64 {
    self.sequence
  }

  /// Returns the sanitized harness event discriminator.
  pub(crate) fn event_type(&self) -> &str {
    &self.event_type
  }

  /// Returns the complete sanitized JSON object.
  pub(crate) fn value(&self) -> &Value {
    &self.value
  }

  /// Identifies the validated terminal state without reparsing JSON.
  pub(crate) fn terminal(&self) -> Option<TerminalEvent> {
    self.terminal
  }
}

/// A bounded sanitization failure that never includes harness or secret data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SanitizationError {
  /// Two distinct object names became identical after secret replacement.
  KeyCollision,
}

impl fmt::Display for SanitizationError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::KeyCollision => formatter.write_str("sanitizing a Codex object produced duplicate field names"),
    }
  }
}

impl Error for SanitizationError {}

/// Command-scoped sanitizer built only from variables marked as secret.
///
/// It reuses the plugin SDK's scalar collection and text-matching rules, so
/// plugin logs, harness stderr, event responses, structured results, and trace
/// records apply the same redaction semantics.
pub(crate) struct RunSanitizer {
  redactions: Vec<Redaction>,
}

impl RunSanitizer {
  /// Resolves every scalar leaf of the named secret variables.
  ///
  /// Missing names and non-secret variables are intentionally ignored, matching
  /// the host logger's behavior. Empty strings and JSON null are not useful
  /// secret matchers and are likewise omitted by the shared collector.
  pub(crate) fn from_variables(variables: &HashMap<String, Value>, secret_variables: &[String]) -> Self {
    Self {
      redactions: collect_variable_redactions(variables, secret_variables),
    }
  }

  /// Consumes a raw event and removes secrets before it can be normalized or retained.
  pub(crate) fn sanitize_event(&self, event: HarnessEvent) -> Result<SanitizedEvent, SanitizationError> {
    let (sequence, mut event_type, mut value, terminal) = event.into_parts();
    if !self.redactions.is_empty() {
      self.sanitize_value_in_place(&mut value)?;
      event_type = self.sanitize_text(&event_type);
    }
    Ok(SanitizedEvent {
      sequence,
      event_type,
      value,
      terminal,
    })
  }

  /// Removes secret scalars from an owned structured result or record value.
  #[cfg(test)]
  pub(crate) fn sanitize_value(&self, mut value: Value) -> Result<Value, SanitizationError> {
    if !self.redactions.is_empty() {
      self.sanitize_value_in_place(&mut value)?;
    }
    Ok(value)
  }

  /// Removes secret scalars from stderr, diagnostics, and other free text.
  pub(crate) fn sanitize_text(&self, value: &str) -> String {
    redact(value, &self.redactions)
  }

  fn sanitize_value_in_place(&self, value: &mut Value) -> Result<(), SanitizationError> {
    match value {
      Value::Null => {},
      Value::Bool(_) | Value::Number(_) => {
        let original = value.to_string();
        let sanitized = self.sanitize_text(&original);
        if sanitized != original {
          *value = Value::String(sanitized);
        }
      },
      Value::String(text) => *text = self.sanitize_text(text),
      Value::Array(values) => {
        for value in values {
          self.sanitize_value_in_place(value)?;
        }
      },
      Value::Object(values) => self.sanitize_object(values)?,
    }
    Ok(())
  }

  fn sanitize_object(&self, values: &mut Map<String, Value>) -> Result<(), SanitizationError> {
    let original = std::mem::take(values);
    for (name, mut value) in original {
      self.sanitize_value_in_place(&mut value)?;
      let name = self.sanitize_text(&name);
      if values.insert(name, value).is_some() {
        return Err(SanitizationError::KeyCollision);
      }
    }
    Ok(())
  }
}

#[cfg(test)]
#[path = "sanitization_tests.rs"]
mod tests;
