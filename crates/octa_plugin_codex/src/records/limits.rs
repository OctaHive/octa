//! Allocation-free encoded-size checks for Codex result metadata.
//!
//! Streaming stderr and trace limits are introduced with their actual sinks in
//! OpenSpec task 4.4. Keeping an unused accounting state machine here would
//! create a second, unenforced source of truth.

use std::io;

use anyhow::{bail, Context};
use octa_plugin::protocol::MAX_PLUGIN_FRAME_BYTES;
use serde::Serialize;

/// Maximum UTF-8 bytes in the terminal plain-text response.
pub(super) const MAX_FINAL_MESSAGE_BYTES: usize = 256 * 1024;
/// Maximum encoded bytes in the terminal structured result.
pub(super) const MAX_STRUCTURED_RESULT_BYTES: usize = 256 * 1024;
/// Maximum encoded bytes in terminal usage metadata.
pub(super) const MAX_USAGE_METADATA_BYTES: usize = 4 * 1024;
/// Space reserved for the response discriminator, command identifier, exit
/// code, JSON object framing, and the trailing newline around plugin outputs.
const PLUGIN_RESPONSE_ENVELOPE_RESERVE_BYTES: usize = 64 * 1024;
/// Maximum encoded size of the complete successful plugin output.
///
/// The outer [`octa_plugin::protocol::PluginResponse`] must remain below the
/// host's frame limit. Keeping explicit headroom here makes that relationship
/// visible while the final transport boundary still validates the exact frame.
pub(super) const MAX_PLUGIN_OUTPUT_BYTES: usize = MAX_PLUGIN_FRAME_BYTES - PLUGIN_RESPONSE_ENVELOPE_RESERVE_BYTES;

/// Validates an encoded JSON size without materializing another byte vector.
pub(super) fn validate_encoded_size(kind: &str, value: &impl Serialize, maximum: usize) -> anyhow::Result<()> {
  match encoded_size_within(value, maximum).with_context(|| format!("failed to encode {kind}"))? {
    true => Ok(()),
    false => bail!("{kind} exceeds {maximum} encoded bytes"),
  }
}

fn encoded_size_within(value: &impl Serialize, maximum: usize) -> Result<bool, serde_json::Error> {
  let mut counter = EncodedSizeCounter::new(maximum);
  let result = serde_json::to_writer(&mut counter, value);
  if counter.exceeded {
    return Ok(false);
  }
  result.map(|_| true)
}

struct EncodedSizeCounter {
  remaining: usize,
  exceeded: bool,
}

impl EncodedSizeCounter {
  fn new(maximum: usize) -> Self {
    Self {
      remaining: maximum,
      exceeded: false,
    }
  }
}

impl io::Write for EncodedSizeCounter {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    if bytes.len() > self.remaining {
      self.exceeded = true;
      return Err(io::Error::other("encoded size limit exceeded"));
    }
    self.remaining -= bytes.len();
    Ok(bytes.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn encoded_size_accepts_the_exact_boundary_without_allocating() {
    assert!(encoded_size_within(&"x", 3).unwrap());
    assert!(!encoded_size_within(&"x", 2).unwrap());
    assert!(validate_encoded_size("fixture", &"x", 2)
      .unwrap_err()
      .to_string()
      .contains("exceeds 2 encoded bytes"));

    let mut counter = EncodedSizeCounter::new(0);
    std::io::Write::flush(&mut counter).unwrap();
  }
}
