use serde_json::json;

use super::*;

fn decode_chunks(chunks: &[&[u8]]) -> Result<Vec<HarnessEvent>, EventDecodeError> {
  let mut decoder = EventDecoder::new();
  let mut events = Vec::new();
  for chunk in chunks {
    let mut input = *chunk;
    while !input.is_empty() {
      if let Some(event) = decoder.next_event(&mut input)? {
        events.push(event);
      }
    }
  }
  if let Some(event) = decoder.finish()? {
    events.push(event);
  }
  Ok(events)
}

#[test]
fn decodes_split_unicode_and_unknown_events_in_wire_order() {
  let stream = concat!(
    "{\"type\":\"thread.started\",\"thread_id\":\"тема\"}\n",
    "{\"type\":\"future.additive\",\"text\":\"готово\"}\n",
    "{\"type\":\"turn.completed\",\"usage\":{}}\n"
  )
  .as_bytes();
  let first_unicode_byte = stream.iter().position(|byte| *byte >= 0x80).unwrap();
  let chunks = [
    &stream[..7],
    &stream[7..first_unicode_byte + 1],
    &stream[first_unicode_byte + 1..stream.len() - 2],
    &stream[stream.len() - 2..],
  ];

  let events = decode_chunks(&chunks).unwrap();

  assert_eq!(events.len(), 3);
  assert_eq!(events.iter().map(|event| event.sequence).collect::<Vec<_>>(), [0, 1, 2]);
  assert_eq!(
    events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>(),
    ["thread.started", "future.additive", "turn.completed"]
  );
  assert_eq!(events[0].value["thread_id"], "тема");
  assert_eq!(events[1].value["text"], "готово");
  assert_eq!(events[2].terminal, Some(TerminalEvent::Completed));
}

#[test]
fn accepts_crlf_and_an_unterminated_terminal_frame() {
  let events = decode_chunks(&[
    b"{\"type\":\"turn.started\"}\r\n",
    b"{\"type\":\"turn.failed\",\"error\":{\"message\":\"nope\"}}",
  ])
  .unwrap();

  assert_eq!(events.len(), 2);
  assert_eq!(events[1].terminal, Some(TerminalEvent::Failed));
}

#[test]
fn rejects_malformed_json_and_invalid_envelopes_without_echoing_input() {
  let long_type = "x".repeat(MAX_EVENT_TYPE_BYTES + 1);
  let mut long_type_frame = serde_json::to_vec(&json!({"type": long_type})).unwrap();
  long_type_frame.push(b'\n');
  let cases = [
    (
      b"not-secret-json\n".to_vec(),
      EventDecodeError::MalformedJson { sequence: 0 },
    ),
    (
      b"[]\n".to_vec(),
      EventDecodeError::InvalidEnvelope {
        sequence: 0,
        reason: "the frame must be a JSON object",
      },
    ),
    (
      b"{\"secret\":\"must-not-appear\"}\n".to_vec(),
      EventDecodeError::InvalidEnvelope {
        sequence: 0,
        reason: "type must be a string",
      },
    ),
    (
      b"{\"type\":\"\"}\n".to_vec(),
      EventDecodeError::InvalidEnvelope {
        sequence: 0,
        reason: "type must not be empty",
      },
    ),
    (
      long_type_frame,
      EventDecodeError::InvalidEnvelope {
        sequence: 0,
        reason: "type is too long",
      },
    ),
    (
      b"{\"type\":\"bad\\nkind\"}\n".to_vec(),
      EventDecodeError::InvalidEnvelope {
        sequence: 0,
        reason: "type contains a control character",
      },
    ),
  ];

  for (frame, expected) in cases {
    let error = EventDecoder::new().next_event(&mut frame.as_slice()).unwrap_err();
    assert_eq!(error, expected);
    assert!(!error.to_string().contains("must-not-appear"));
  }
}

#[test]
fn rejects_an_oversized_frame_before_a_line_terminator_arrives() {
  let mut decoder = EventDecoder::new();
  let frame = vec![b'x'; MAX_EVENT_FRAME_BYTES];
  decoder.next_event(&mut frame.as_slice()).unwrap();

  assert_eq!(
    decoder.next_event(&mut b"x".as_slice()).unwrap_err(),
    EventDecodeError::FrameTooLarge {
      limit: MAX_EVENT_FRAME_BYTES
    }
  );
}

#[test]
fn rejects_duplicate_terminal_events() {
  let mut decoder = EventDecoder::new();
  decoder
    .next_event(&mut b"{\"type\":\"turn.completed\"}\n".as_slice())
    .unwrap();

  assert_eq!(
    decoder
      .next_event(&mut b"{\"type\":\"turn.failed\"}\n".as_slice())
      .unwrap_err(),
    EventDecodeError::DuplicateTerminal { first: 0, duplicate: 1 }
  );
}

#[test]
fn rejects_non_terminal_events_after_the_terminal_event() {
  let mut decoder = EventDecoder::new();
  decoder
    .next_event(&mut b"{\"type\":\"turn.completed\"}\n".as_slice())
    .unwrap();

  assert_eq!(
    decoder
      .next_event(&mut b"{\"type\":\"future.event\"}\n".as_slice())
      .unwrap_err(),
    EventDecodeError::EventAfterTerminal { terminal: 0, event: 1 }
  );
}

#[test]
fn rejects_a_stream_without_a_terminal_event() {
  let mut decoder = EventDecoder::new();
  let mut events = Vec::new();
  let mut input = b"{\"type\":\"thread.started\"}\n{\"type\":\"item.completed\"}\n".as_slice();
  while !input.is_empty() {
    if let Some(event) = decoder.next_event(&mut input).unwrap() {
      events.push(event);
    }
  }
  assert_eq!(events.len(), 2);

  assert_eq!(decoder.finish().unwrap_err(), EventDecodeError::MissingTerminal);
}

#[test]
fn rejects_an_unrepresentable_event_sequence() {
  let mut decoder = EventDecoder {
    next_sequence: u64::MAX,
    ..EventDecoder::new()
  };

  assert_eq!(
    decoder
      .next_event(&mut b"{\"type\":\"future.event\"}\n".as_slice())
      .unwrap_err(),
    EventDecodeError::TooManyEvents
  );
}

#[test]
fn protocol_errors_have_bounded_context_only() {
  let errors = [
    EventDecodeError::FrameTooLarge { limit: 1 },
    EventDecodeError::MalformedJson { sequence: 2 },
    EventDecodeError::InvalidEnvelope {
      sequence: 3,
      reason: "type must be a string",
    },
    EventDecodeError::DuplicateTerminal { first: 4, duplicate: 5 },
    EventDecodeError::EventAfterTerminal { terminal: 6, event: 7 },
    EventDecodeError::MissingTerminal,
    EventDecodeError::TooManyEvents,
  ];

  for error in errors {
    let diagnostic = error.to_string();
    assert!(diagnostic.len() < 128);
    assert!(!diagnostic.contains('{'));
  }
}

#[test]
fn retains_the_original_terminal_payload() {
  let [event] = decode_chunks(&[b"{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7}}\n"])
    .unwrap()
    .try_into()
    .unwrap();

  assert_eq!(
    event.value,
    json!({"type": "turn.completed", "usage": {"input_tokens": 7}})
  );
}
