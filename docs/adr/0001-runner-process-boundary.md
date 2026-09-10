# ADR 0001: Runner process boundary

- Status: accepted
- Date: 2026-09-08

## Context

Octa must remain useful as a local task runner while the future agent and
server live in another repository. Importing Octa's internal Rust crates into
the agent would couple their releases and make terminal-oriented CLI behavior
part of distributed execution.

## Decision

Octa provides two binaries backed by the same concrete `octa-runtime`:

- `octa` owns interactive CLI parsing and human-readable presentation;
- `octa-runner` owns the versioned, non-interactive JSON Lines process
  protocol used by an agent.

The caller prepares an absolute workspace and starts a pinned runner binary.
The runner loads the Octafile from that workspace, builds the DAG, executes it,
and returns ordered runtime events plus structured terminal results.

The runner request describes execution. It does not contain repository
checkout, job leases, sandbox configuration, resource scheduling, server
credentials, or artifact upload endpoints. Those remain agent concerns.

Plugins and their child processes run inside the job boundary selected by the
agent. Plugin capabilities are descriptive metadata, not a security boundary.
Octa resolves secret references itself; the agent supplies workload identity
and network access but does not put resolved secret values in the runner
protocol.

Internal Rust APIs and unreleased wire formats may change incompatibly when
that produces a simpler final design. Published runner, event, Octafile, and
plugin protocol versions are negotiated independently.

## Consequences

- The agent depends only on released binaries and documented protocols.
- Local and agent execution cannot drift into separate implementations.
- `octa-runtime` has exactly two consumers: the CLI and runner.
- Octa has no dependency on agent or server code.
- New distributed features must be expressed as execution inputs, events, or
  results only when they belong to Octa's execution responsibility.
