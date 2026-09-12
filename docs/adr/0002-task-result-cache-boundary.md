# ADR 0002: task-result cache ownership and trust boundary

## Status

Accepted for implementation. Amended before Phase 3 to replace legacy
freshness with the result cache rather than retaining two skip mechanisms.

## Context

Octa already has workspace-local freshness checks and execution-local result
reuse. Neither mechanism stores task outputs, nor can either identify the same
action across workspaces or agents. Keeping freshness beside a result cache
would duplicate file discovery, hashing, output inspection, and skip decisions.
OctaCity needs distributed reuse without moving task semantics, filesystem
restoration, or integrity decisions into the server.

Shell tasks may read undeclared files, inherited environment variables, the
network, clocks, random sources, and host tools. No cache can infer this full
dependency set reliably. A task opting into result caching therefore declares
the filesystem and inherited-environment portion of its input contract.

Cache metadata and bundles cross machine and authorization boundaries. They
must be treated as untrusted even when read from a local cache, because local
state can be truncated, concurrently modified, or left behind by an older
binary.

## Decision

Octa owns cache semantics end to end:

- it constructs a versioned action identity from declared semantic inputs;
- it hashes inputs by content and rechecks them before publication or restore;
- it captures only declared output roots;
- it verifies, stages, and atomically replaces outputs;
- it decides whether a task is cacheable and reports safe miss reasons;
- it exposes one store boundary used by local and remote implementations.

OctaCity owns authorization, namespace isolation, quotas, retention, remote
transport, and production blob storage. Octa talks to OctaCity through a
versioned cache HTTP protocol and never imports OctaCity crates or an S3 SDK.

The action cache maps an immutable action digest to an immutable result record.
The CAS stores deterministic output bundles. Blobs are published before action
records, and all writes are create-if-absent. Conflicting results for one
action are reported as nondeterminism and never overwrite the first result.

`octa-cache-protocol` is the lowest dependency. It contains bounded wire types,
portable paths, schemas, and the explicit `octa.action.v1` canonical encoding.
It contains no filesystem, executor, Tokio, compression, or HTTP code.

Each task has one `files` contract. `files.inputs` is required for a cacheable
task, though it may be `[]`; `files.outputs` contains exact relative roots and
may be empty when only the logical result is reusable. `cache` enables result
caching and contains policy inputs such as inherited environment names and an
optional salt, without repeating file paths. Inherited process variables affect
the key only when named in `cache.environment`. Task-defined public variables
and environment values are included automatically. Secret-bearing tasks are not
cacheable in version one.

Legacy `sources`, `output`, `source_strategy`, persistent freshness state, and
the sled fingerprint database are removed without compatibility translation.
Watch mode and result caching share the `octa-cache` file selector and
`InputSnapshotter`. BLAKE3 content identity is mandatory; timestamp and custom
fingerprint strategies are not configuration options. Metadata may only serve
as a fail-safe internal acceleration hint and never replaces a content digest.

An unchanged workspace is a materialized local cache hit. Current declared
outputs are compared using the canonical output-tree digest; matching outputs
are left untouched, while missing or modified roots are atomically restored.
`ActionResultV1.output_bundle` is absent exactly when `files.outputs` is empty.
Top-level structured `outputs`, artifacts, and reports remain logical result
metadata and can be replayed by the same action result.

## Threat model and required failure posture

The implementation assumes an attacker may control repository content, cache
responses, archive bytes, file names inside bundles, symlinks, compression
ratios, and task output volume. Repository content cannot select host cache
credentials, cache endpoints, namespaces, or executable cache providers.

The implementation must defend against:

- false hits caused by omitted key fields or mutable inputs;
- path traversal and absolute, drive-prefixed, or non-portable paths;
- symlink escapes and special filesystem entries;
- truncated, corrupt, oversized, or decompression-bomb bundles;
- action records becoming visible before their blobs are durable;
- partial workspace replacement after cancellation or process death;
- concurrent equal or conflicting publishers;
- secret values entering keys, metadata, diagnostics, bundles, or logs;
- remote failures turning an optimization into a false successful build.

Integrity or restore failure is a cache miss followed by normal execution in
the default mode. It is never a cache hit. Publication failure does not change
a successful task result. Explicit controlled environments may later select a
required-cache policy.

## Consequences

Action keys remain stable across absolute workspace locations and independent
implementations. Local and remote storage share correctness tests. The first
implementation accepts the cost of content hashing, staging, fsync, and output
replacement in exchange for fail-closed behavior.

Ordinary execution of tasks without a `cache` block does not construct cache
objects, walk inputs, or touch cache storage. Explicit watch mode may snapshot a
declared `files.inputs` contract. This boundary is a performance invariant and
is measured against the pre-cache CLI and runner baselines.

The design deliberately excludes direct S3 access, agent-to-agent transfer,
distributed execution leases, arbitrary cache-provider plugins, inferred shell
dependencies, and timestamp-only identity.
