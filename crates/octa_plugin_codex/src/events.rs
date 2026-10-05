//! Bounded decoding and normalization of the Codex JSONL event stream.
//!
//! This module turns untrusted machine output into the plugin's small internal
//! event model. It does not own the child process or persist audit records.
