//! Ownership and termination of the Codex child process tree.
//!
//! This module is the single owner of process startup, standard streams,
//! cancellation, and bounded graceful-to-forced shutdown. It deliberately
//! models the concrete Codex CLI instead of exposing a backend trait.
