//! Bounded decoding and normalization of the Codex JSONL event stream.
//!
//! Codex stdout is untrusted protocol input. [`EventDecoder`] accepts arbitrary
//! byte chunks, assembles at most one bounded frame, validates the common event
//! envelope, and assigns a local sequence number before the event reaches
//! logging or durable records. It deliberately retains unknown non-terminal
//! events so newer additive Codex releases do not break older Octa plugins.
//!
//! A stream is valid only when it contains exactly one `turn.completed` or
//! `turn.failed` event and nothing follows that terminal event. Before an event
//! can be normalized or serialized, [`crate::sanitization::RunSanitizer`]
//! consumes it and removes every resolved secret scalar from its complete JSON
//! tree. Process ownership and retention limits remain outside this module.

use std::{error::Error, fmt};

use serde_json::Value;

mod normalize;

pub(crate) use normalize::normalize_activity;

/// Maximum JSON bytes in one Codex event, excluding the line terminator.
///
/// The decoder checks this while bytes arrive, so a missing newline cannot
/// grow the pending-frame allocation without bound.
pub(crate) const MAX_EVENT_FRAME_BYTES: usize = 1024 * 1024;

/// Maximum UTF-8 bytes in the event discriminator retained by the plugin.
const MAX_EVENT_TYPE_BYTES: usize = 128;

/// One validated Codex event in its original JSON shape.
///
/// The raw value is retained for the normalization and sanitization stages.
/// `sequence` is assigned by arrival order and never trusted from the harness.
#[cfg_attr(test, derive(Debug))]
pub(crate) struct HarnessEvent {
  sequence: u64,
  event_type: String,
  value: Value,
  terminal: Option<TerminalEvent>,
}

impl HarnessEvent {
  /// Transfers the validated decoder state to the run sanitizer.
  pub(crate) fn into_parts(self) -> (u64, String, Value, Option<TerminalEvent>) {
    (self.sequence, self.event_type, self.value, self.terminal)
  }
}

/// Harness events that close a Codex turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalEvent {
  /// Codex completed the turn and supplied its normal terminal payload.
  Completed,
  /// Codex terminated the turn with a harness-level failure.
  Failed,
}

/// A bounded protocol failure that never embeds untrusted event contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EventDecodeError {
  /// The current frame exceeded [`MAX_EVENT_FRAME_BYTES`].
  FrameTooLarge { limit: usize },
  /// A complete frame was not valid JSON.
  MalformedJson { sequence: u64 },
  /// The JSON value was not an object with a safe string `type` field.
  InvalidEnvelope { sequence: u64, reason: &'static str },
  /// A second terminal event followed the first one.
  DuplicateTerminal { first: u64, duplicate: u64 },
  /// A non-terminal event followed the terminal event.
  EventAfterTerminal { terminal: u64, event: u64 },
  /// End-of-stream arrived without a terminal event.
  MissingTerminal,
  /// The stream contained more events than can be sequenced safely.
  TooManyEvents,
}

impl fmt::Display for EventDecodeError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::FrameTooLarge { limit } => write!(formatter, "Codex event exceeds the {limit}-byte frame limit"),
      Self::MalformedJson { sequence } => write!(formatter, "Codex event {sequence} is not valid JSON"),
      Self::InvalidEnvelope { sequence, reason } => {
        write!(formatter, "Codex event {sequence} has an invalid envelope: {reason}")
      },
      Self::DuplicateTerminal { first, duplicate } => write!(
        formatter,
        "Codex stream contains terminal events at positions {first} and {duplicate}"
      ),
      Self::EventAfterTerminal { terminal, event } => write!(
        formatter,
        "Codex event {event} follows the terminal event at position {terminal}"
      ),
      Self::MissingTerminal => formatter.write_str("Codex stream ended without a terminal event"),
      Self::TooManyEvents => formatter.write_str("Codex stream contains too many events"),
    }
  }
}

impl Error for EventDecodeError {}

/// Incrementally frames and validates one Codex JSONL stream.
///
/// [`Self::next_event`] consumes at most one complete event from a caller-owned
/// input slice. Returning one event at a time lets an async coordinator process
/// and forward it before parsing the next frame, without buffering a batch or
/// forcing asynchronous work into a callback. [`Self::finish`] consumes the
/// decoder, accepts a final JSON line without a trailing newline, and verifies
/// the terminal-event invariant.
#[derive(Debug, Default)]
pub(crate) struct EventDecoder {
  pending: Vec<u8>,
  next_sequence: u64,
  terminal_sequence: Option<u64>,
}

impl EventDecoder {
  /// Creates an empty decoder for a single Codex invocation.
  pub(crate) fn new() -> Self {
    Self::default()
  }

  /// Consumes bytes through the first line boundary and returns one event.
  ///
  /// The input slice is advanced past consumed bytes. Callers repeat this
  /// method until the slice is empty, processing each returned event before
  /// requesting another one.
  pub(crate) fn next_event(&mut self, bytes: &mut &[u8]) -> Result<Option<HarnessEvent>, EventDecodeError> {
    if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
      self.extend_pending(&bytes[..newline])?;
      *bytes = &bytes[newline + 1..];
      return self.decode_pending().map(Some);
    }
    self.extend_pending(bytes)?;
    *bytes = &[];
    Ok(None)
  }

  /// Finalizes the stream and returns an optional unterminated last event.
  pub(crate) fn finish(mut self) -> Result<Option<HarnessEvent>, EventDecodeError> {
    let event = if self.pending.is_empty() {
      None
    } else {
      Some(self.decode_pending()?)
    };
    if self.terminal_sequence.is_none() {
      return Err(EventDecodeError::MissingTerminal);
    }
    Ok(event)
  }

  fn extend_pending(&mut self, bytes: &[u8]) -> Result<(), EventDecodeError> {
    if bytes.len() > MAX_EVENT_FRAME_BYTES.saturating_sub(self.pending.len()) {
      return Err(EventDecodeError::FrameTooLarge {
        limit: MAX_EVENT_FRAME_BYTES,
      });
    }
    self.pending.extend_from_slice(bytes);
    Ok(())
  }

  fn decode_pending(&mut self) -> Result<HarnessEvent, EventDecodeError> {
    let sequence = self.next_sequence;
    let mut frame = std::mem::take(&mut self.pending);
    if frame.last() == Some(&b'\r') {
      frame.pop();
    }
    let value = serde_json::from_slice::<Value>(&frame).map_err(|_| EventDecodeError::MalformedJson { sequence })?;
    let object = value.as_object().ok_or(EventDecodeError::InvalidEnvelope {
      sequence,
      reason: "the frame must be a JSON object",
    })?;
    let event_type = object
      .get("type")
      .and_then(Value::as_str)
      .ok_or(EventDecodeError::InvalidEnvelope {
        sequence,
        reason: "type must be a string",
      })?;
    if event_type.is_empty() {
      return Err(EventDecodeError::InvalidEnvelope {
        sequence,
        reason: "type must not be empty",
      });
    }
    if event_type.len() > MAX_EVENT_TYPE_BYTES {
      return Err(EventDecodeError::InvalidEnvelope {
        sequence,
        reason: "type is too long",
      });
    }
    if event_type.chars().any(char::is_control) {
      return Err(EventDecodeError::InvalidEnvelope {
        sequence,
        reason: "type contains a control character",
      });
    }

    let terminal = match event_type {
      "turn.completed" => Some(TerminalEvent::Completed),
      "turn.failed" => Some(TerminalEvent::Failed),
      _ => None,
    };
    if let Some(first) = self.terminal_sequence {
      return Err(if terminal.is_some() {
        EventDecodeError::DuplicateTerminal {
          first,
          duplicate: sequence,
        }
      } else {
        EventDecodeError::EventAfterTerminal {
          terminal: first,
          event: sequence,
        }
      });
    }
    if terminal.is_some() {
      self.terminal_sequence = Some(sequence);
    }
    self.next_sequence = sequence.checked_add(1).ok_or(EventDecodeError::TooManyEvents)?;

    let event_type = event_type.to_owned();
    frame.clear();
    self.pending = frame;

    Ok(HarnessEvent {
      sequence,
      event_type,
      value,
      terminal,
    })
  }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
