# Distributed caching implementation plan

## Status

This document defines the intended cache architecture and implementation order.
The completed implementation boundary is tracked here so later work does not
silently skip a phase:

- [x] Phase 0: ADR, threat model, and baseline.
- [x] Phase 1: protocol and canonical identity.
- [x] Phase 2: input snapshot and output bundle.
- [x] Pre-Phase 3 unification amendment: optional filesystem output bundle.
- [x] Phase 3: local CAS and restore transactions.
- [x] Phase 4: Octafile, planner, and executor integration.
- [x] Phase 5: runtime, CLI, and runner integration.
- [ ] Phase 6: plugin-provided filesystem contracts.
- [ ] Phase 7: distributed tool caches.
- [ ] Phase 8: remote task-result cache HTTP client.
- [ ] Phase 9: correctness and security hardening.
- [ ] Phase 10: performance validation.

Phases 1–5 produced the reusable protocol, snapshot, bundle, local CAS,
transactional restore, executor integration, operator-facing CLI, and the
versioned runner boundary. The pre-release
`ActionResultV1` represents results without filesystem outputs without a
sentinel blob. Phase 4 replaced, rather than retained beside the cache, the
legacy freshness implementation. Phase 5 makes CLI and headless runner use the
same runtime composition and action semantics. Phase 6 removes unnecessary
manual file declarations from specialized plugin tasks, Phase 7 adds
fine-grained caches used while a task executes, and remote task-result transport
remains Phase 8.

## Goal

Octa must be able to reuse a successful task result in another workspace and,
eventually, on another OctaCity agent. Octa owns the meaning of a cacheable
task: it computes the action identity, captures outputs, restores them, and
returns cached structured results. Specialized plugins may additionally
describe that task contract and attach fine-grained tool caches such as a
compiler object cache or BuildKit layer cache. OctaCity will later provide
shared storage, authorization, quotas, and cross-agent coordination.

The target guarantees are:

- no false cache hits when a task obeys its declared input contract;
- the same task in different absolute workspaces receives the same action key;
- a corrupt, incomplete, or unavailable cache never becomes a successful
  cached execution;
- publishing and restoring a result cannot leave a partially valid action or a
  partially replaced workspace;
- every cache miss has a machine-readable reason that can be inspected without
  exposing secrets;
- tasks that do not enable result caching retain their current performance.

Arbitrary shell commands can read undeclared files, the network, time, random
data, or tools installed on the host. Octa cannot infer all such inputs. The
guarantee therefore applies to the explicit cache contract; enabling caching is
an assertion that the declared inputs and execution environment completely
describe the result.

## State before Phase 4

Octa had three mechanisms that were sometimes described as caches:

1. Persistent freshness state in `octa-executor`. It fingerprints `sources`,
   checks `output`, and records state in `sled`. It decides whether work should
   run in the current workspace, but does not save output contents.
2. The execution-local `run: once/changed` map. It reuses stdout and structured
   outputs only within one execution and does not persist files.
3. Monorepo discovery metadata. It avoids rescanning unchanged directory
   structure and is not a build-result cache.

The persistent freshness fingerprint is intentionally workspace-local. Its key
contains the canonical absolute root, and its current path hashing includes
native absolute paths. It cannot be used as a distributed action identity. It
also duplicates the new input discovery, hashing, output inspection, and skip
decision.

The task-result cache therefore replaces persistent freshness instead of living
beside it. A task that has a cache declaration has one action identity and one
lookup decision. An unchanged task in its current workspace is a materialized
local cache hit; a missing or modified output tree is restored from the same
action result. The execution-local `run: once/changed` rule remains a control-flow
rule, and monorepo discovery remains an unrelated metadata optimization.

## Responsibility boundary

```text
Octafile task file contract + cache declaration
                       |
                       v
                 octa-executor
                 |- action identity
                 |- lookup decision
                 |- output capture
                 `- safe restore
                       |
                       v
                  CacheStore
                  /        \
                 v          v
          LocalCacheStore  RemoteCacheStore
                                |
                                v
                         OctaCity cache API
                                |
                                v
                         S3-compatible CAS
```

### Octa responsibilities

- define one task filesystem contract shared by watch and result caching;
- define task-cache policy without repeating filesystem paths;
- determine whether a task is cacheable;
- compute a portable action digest;
- identify and hash filesystem inputs;
- capture declared filesystem outputs;
- retain safe structured outputs and resource registrations;
- restore a result atomically;
- verify all downloaded content;
- expose cache events, outcomes, and miss diagnostics;
- provide local and HTTP cache clients usable by both the CLI and runner.

### OctaCity responsibilities

- authenticate cache sessions;
- authorize project and organization namespaces;
- store action records and immutable blobs;
- issue short-lived, job-scoped access;
- enforce remote quotas and retention;
- collect cache metrics;
- later coordinate concurrent agents building the same action.

Octa must not import OctaCity crates or server DTOs. Octa must not contain an S3
SDK. It talks to a versioned cache HTTP protocol; the server decides how and
where objects are stored.

## Cache granularity and DAG integration

The unit of result caching is one logical task invocation, not an individual
command node. A cacheable task may contain sequential or parallel plugin
commands, per-command conditions, shared outputs, structured exports, resource
registrations, and deferred cleanup. Version one rejects a cache boundary
around nested task calls: their resolved definitions and child result tree do
not yet have a canonical parent action identity. Cache the executable leaf
tasks instead. Caching individual command nodes would expose intermediate
filesystem states and allow overlapping output ownership.

The logical order is:

```text
dependencies
    |
    v
conditions and preconditions
    |
    v
input snapshot and cache lookup
    |-------------------------------|
    | hit                           | miss
    v                               v
verify and restore             execute task body
    |                               |
    |                         deferred commands
    |                               |
    |                         register resources
    |                               |
    |                         recheck inputs
    |                               |
    |                         capture and publish
    |-------------------------------|
                    |
                    v
              task completion
```

The planner should add two task-level hidden actions:

- `CacheLookup`, which computes the action, performs lookup, restores a hit,
  and publishes a shared hit/miss decision;
- `CacheFinalize`, which returns the restored result on a hit or captures and
  publishes a successfully executed miss.

Command and deferred nodes observe the shared decision and do not execute on a
hit. A cache hit is a successful task with a `cached` outcome, not a condition
skip. There is no independent freshness decision.

Dependencies execute before lookup because they can produce declared input
files and structured values used by the task. Only dependency values actually
resolved into the task identity are included; adding every dependency action
digest would cause misses when an irrelevant dependency output changes.

## Octafile model

The filesystem contract is declared once and is independent from cache policy.
Result caching is explicitly enabled by the presence of `cache`:

```yaml
tasks:
  build:
    files:
      inputs:
        - src/**/*
        - Cargo.toml
        - Cargo.lock
      outputs:
        - target/release/octa
        - generated
    cache:
      environment:
        - RUSTFLAGS
        - CARGO_PROFILE_RELEASE_LTO
      salt: rust-release-v1
    shell: cargo build --release
```

Rules:

- the presence of `cache` enables task-result caching;
- `files.inputs` is required for a cacheable task and may be explicitly empty;
- `files.outputs` may be empty for a task whose reusable result contains only
  success, bounded stdout, or public structured outputs;
- `files.inputs` use ordered include/exclude and hierarchical `.octaignore`
  semantics implemented only by `octa-cache`;
- `files.outputs` are exact relative files or directory roots, not globs;
- output roots must not overlap each other;
- inputs and output roots must not overlap;
- every file-contract path must remain inside the workspace and use a portable
  workspace-relative representation;
- absolute and external inputs are rejected;
- explicitly configured task variables and environments are included
  automatically;
- `environment` names inherited process variables that affect the output;
- `salt` is an optional user-controlled invalidation value.

Exact output roots are required for reliable replacement. A glob describes
files that exist now, but cannot describe stale files that should be removed
when an older workspace is restored.

The old `sources`, `output`, and `source_strategy` fields are removed. There is
no compatibility translation and no timestamp/custom fingerprint mode. Content
identity is always the canonical BLAKE3 input-tree digest. This deliberately
prevents local and distributed execution from disagreeing about whether the
same task is current.

`watch` consumes `files.inputs` through the same snapshotter. A task may declare
`files` for watch without enabling result caching; without `cache`, the file
contract alone never causes execution to be skipped.

Top-level `outputs` continues to mean structured values exported from named
plugin steps. It is not a filesystem declaration. Unlike legacy freshness, the
result cache can reproduce those public values together with filesystem outputs,
so the two forms may be used by the same task. Artifacts and reports remain
registrations and must resolve beneath a declared `files.outputs` root.

`run: once/changed` remains an execution-local invocation rule and is not used
as persistent cache configuration. Its internal storage is named invocation
reuse rather than cache to keep the distinction explicit.

## Environment-specific cache configuration

Backend, credentials, and operational limits do not belong in source-controlled
Octafile configuration. A separate profile is used by the local CLI or created
for a runner job by the agent:

```toml
mode = "read_write"
namespace = "project/example"

[local]
directory = "/var/cache/octa"
max_bytes = 10737418240

[remote]
url = "https://cache.example.com"
token_file = "/run/secrets/octa-cache-token"
request_timeout_seconds = 30
max_parallel_transfers = 8

[environment]
identity = "linux-amd64-rust-1.98"
```

All sizes, timeouts, concurrency limits, compression settings, and watermarks
must be profile fields with documented centralized defaults. Executor code must
not contain scattered operational constants.

For an OCI job, the environment identity includes the immutable image digest.
For a Native job, remote cache writes require an explicit environment digest
provided by the local profile or agent. OS and architecture alone do not prove
that compilers and system libraries are equal.

Tokens are read from a file and are never serialized in Octafile, runner events,
results, action descriptors, or debug output.

## Action identity

The action key is a digest of an explicitly versioned typed descriptor:

```rust
pub struct ActionDescriptorV1 {
  pub key_format: u16,
  pub task_definition: Digest,
  pub input_root: Digest,
  pub working_directory: RelativePath,
  pub variables: Digest,
  pub environment: Digest,
  pub runtime: RuntimeIdentity,
  pub plugins: Vec<PluginIdentity>,
  pub arguments: Vec<String>,
  pub timeout: Option<Duration>,
  pub salt: Option<String>,
}
```

```text
ActionDigest = BLAKE3(canonical(ActionDescriptorV1))
```

### Included identity

- cache-key format version;
- semantic task definition;
- resolved public task variables and configured environment;
- explicitly selected inherited environment variables;
- command arguments;
- relative task working directory;
- input-tree digest;
- operating system and architecture;
- immutable OCI image digest or Native environment digest;
- digest and protocol version of every plugin used by the task;
- task timeout;
- optional user salt.

Timeout is semantic. A result produced with a longer timeout must not hide that
the same command fails under a shorter configured timeout. The Remote Execution
API follows the same rule for its Action model.

### Excluded identity

- absolute workspace path;
- agent, job, attempt, and run identifiers;
- file timestamps;
- cache URL and credentials;
- cache read/write policy;
- presentation, prefix, quiet, and silent settings;
- secret values;
- plugins not used by the task.

Including the complete plugin lock would make an unrelated JUnit plugin update
invalidate a Rust build. The planner must collect the plugin identities used by
the task command, conditions, preconditions, and plugin-backed template
expressions.

### Canonical encoding

Action keys must not depend on arbitrary `serde_json::Value` serialization.
Digest input uses an explicit length-prefixed binary encoding with fixed field
tags and domain separation:

```text
octa.action.v1
octa.input-tree.v1
octa.output-bundle.v1
octa.file.v1
```

Changing a key-relevant field or its encoding requires a new key-format
version. JSON remains suitable for diagnostics and wire DTOs but is not the
canonical byte representation used by the digest.

## Input tree and hashing

### Digest algorithm

BLAKE3 is the required internal cache digest because it is cryptographically
suitable for content addressing, supports efficient parallel hashing, and is
substantially faster than SHA-256 for large build trees. `Digest` includes an
algorithm tag and byte size so the format can evolve.

BLAKE3 content identity is the only task input strategy. Octafile does not offer
timestamp, hash, or custom strategy selection. Timestamps, sizes, file IDs, and
filesystem journals may later accelerate reuse of a previously computed file
digest, but they never replace that digest in the input tree or action key. Any
accelerator must discard uncertain state and fall back to reading the content.

Existing external identities retain their original algorithms. For example, a
SHA-256 plugin or OCI digest is embedded as a tagged external digest rather than
silently reinterpreted as BLAKE3.

### Tree representation

The input root contains stable workspace-relative entries:

```text
directory
|- file: relative path + size + executable bit + content digest
|- symlink: relative path + link target
`- empty directory
```

Rules:

- path separator is `/` in the canonical form;
- entries are sorted by canonical path bytes;
- absolute roots, uid, gid, atime, and mtime are excluded;
- entry type and executable bit are included;
- symlinks are hashed as links and are not followed;
- absolute or workspace-escaping symlinks are rejected;
- sockets, devices, and other special files are rejected;
- cache and watch file contracts require UTF-8 portable paths on every host.

The cache file selector is also the watch selector. Differential tests against
a retained legacy implementation are useful during migration, but the legacy
collector is deleted before Phase 4 is complete; two production selectors are
not accepted as a permanent compatibility layer.

### Mutation detection

Input hashing must detect files that change during the operation:

1. Read metadata before opening the file.
2. Stream and hash its contents.
3. Read metadata again.
4. Retry a bounded number of times if the file changed.
5. Fail the snapshot if the file remains unstable.

After a successful task, Octa recomputes the input root before publication. A
changed digest prevents publishing the result under the old action key. On a
cache hit, inputs are revalidated before the final output replacement.

This prevents a common cache-poisoning race where the action is keyed before an
input changes but the produced output observes the later contents.

### Hashing performance

The first implementation must remain content-based. A persistent
`(mtime, size) -> digest` shortcut is not accepted because content can change
while those values are preserved.

Performance comes from:

- BLAKE3 streaming and parallel hashing;
- a single runtime-wide hashing budget rather than one CPU pool per task;
- bounded open-file concurrency;
- digest reuse for overlapping inputs within one process;
- one content read per inode for hardlinks within a snapshot;
- large reusable read buffers;
- cancellation checks between chunks;
- avoiding a second pattern walk after input discovery.

A persistent metadata index may be considered only after a long-lived watcher
or daemon can provide reliable invalidation and benchmarks demonstrate that
content hashing is the bottleneck. Journal replacement, event overflow, process
restart without continuity, and unsupported/network filesystems must invalidate
the affected index rather than risk a false hit.

## Output bundle

Creating one remote object for every small output file is unsuitable for build
trees containing tens of thousands of entries. Version one uses one
deterministic, content-addressed bundle per task result:

```text
OutputBundleV1
|- canonically ordered entry headers
|- relative path
|- entry type
|- executable mode
|- content length
`- content bytes
```

Properties:

- creation and extraction are streaming;
- memory use is bounded independently of total output size;
- entries use deterministic component-wise tree order, with each directory
  before its contiguous subtree;
- uid, gid, timestamps, and absolute paths are not stored;
- empty directories and relative safe symlinks are preserved;
- Zstandard uses a configurable fast level;
- identity is computed over the canonical uncompressed representation;
- encoding and compression version are stored separately;
- maximum expanded size, file count, path length, and compression ratio are
  enforced.

The uncompressed digest separates semantic content from the compressor version.
Pack sharding or file-level CAS can be added later only if measurements show
that cross-action deduplication is worth the added request and indexing cost.

## Action result

```rust
pub struct ActionResultV1 {
  pub action: Digest,
  pub output_bundle: Option<BlobDescriptor>,
  pub stdout: Option<BoundedString>,
  pub task_outputs: Map<String, Value>,
  pub artifacts: Vec<RegisteredArtifact>,
  pub reports: Vec<RegisteredReport>,
}
```

The result contains only data required to reproduce task semantics:

- declared filesystem outputs;
- bounded stdout used by the existing dependency-result mechanism;
- public structured task outputs;
- artifact and report registrations.

`output_bundle` is absent only when `files.outputs` is empty. A non-empty output
contract requires a bundle even when its roots contain only empty directories.
This is encoded explicitly instead of inventing a sentinel blob.

It does not store or replay the complete stdout/stderr event stream. A hit emits
one cache event and restores the logical task result. Secret structured outputs
make the task non-cacheable in version one.

Every artifact or report registered by a cacheable task must be contained in a
declared cache output root. Otherwise a hit could not reproduce the registered
resource.

Before downloading or replacing outputs on a hit, Octa computes the canonical
tree digest of the current `files.outputs`. If it equals the referenced bundle,
the workspace is already materialized and no files are rewritten. A missing,
extra, or modified entry causes the complete declared roots to be restored. The
inspection and bundle writer share one canonical output-tree implementation.

## Atomic output restoration

Extraction must never write directly over live outputs. Restore uses a durable
transaction:

1. Inspect the live output roots while holding their path-prefix locks.
2. Write and synchronize an immutable intent journal before creating staging.
3. Download and extract the complete bundle into a same-filesystem staging tree.
4. Verify compressed limits, expanded size, BLAKE3, every path, entry type,
   symlink, and permission.
5. Synchronize staging and publish a durable prepared marker. No live output is
   changed before this marker exists.
6. Rename existing output roots into a backup area.
7. Rename staged roots into their final locations.
8. Synchronize the affected parent directories.
9. Publish a durable committed marker.
10. Remove backups, staging, markers, and the intent journal.

At startup, unfinished journals are recovered before new execution. An intent
without a prepared marker only owns disposable staging. A prepared transaction
without a committed marker restores backups, while a committed transaction
retains the new outputs and only removes transaction state. A manual local
workspace must never be left with half of its outputs replaced.

Output roots are locked in deterministic path order while capture or restore is
active. The planner also rejects cacheable tasks whose concurrently executable
output roots overlap.

## Storage model

The design needs one interface because local and remote stores are both planned
real implementations:

```rust
#[async_trait]
pub trait CacheStore: Send + Sync {
  async fn get_action(
    &self,
    namespace: &str,
    action: &Digest,
  ) -> Result<Option<ActionResultV1>>;

  async fn find_missing_blobs(
    &self,
    blobs: &[BlobDescriptor],
  ) -> Result<Vec<BlobDescriptor>>;

  async fn read_blob(
    &self,
    blob: &BlobDescriptor,
  ) -> Result<BlobReader>;

  async fn write_blob_if_absent(
    &self,
    blob: &BlobDescriptor,
    body: BlobReader,
  ) -> Result<WriteOutcome>;

  async fn write_action_if_absent(
    &self,
    namespace: &str,
    result: &ActionResultV1,
  ) -> Result<WriteOutcome>;
}
```

There should not be separate traits for hashing, packing, compression, garbage
collection, filesystem transactions, or every protocol operation.

### Crate boundaries

```text
octa-cache-protocol
        ^
        |
    octa-cache
        ^
        |
  octa-executor
        ^
        |
   octa-runtime
      ^     ^
      |     |
octa-cli  octa-runner

octa-cache-http ------> octa-cache / octa-cache-protocol
```

`octa-cache-protocol` contains wire types, version constants, schemas, limits,
and golden fixtures. It has no executor, filesystem, Tokio, or HTTP dependency.

`octa-cache` contains action identity, input snapshots, bundles, secure restore,
the store interface, local store, layered lookup, and local garbage collection.
It is also the only implementation of ordered file-pattern discovery and
canonical input/output trees. `octa-executor` maps Octafile declarations into
these types; it does not retain a second source collector or path hasher.

`octa-cache-http` contains HTTP connection pooling, authentication, streaming
transfers, retries, and the circuit breaker. Only the composition roots depend
on it; the executor remains transport-independent.

## Local CAS

Suggested layout:

```text
cache/v1/
|- actions/<namespace>/<prefix>/<action-digest>
|- blobs/blake3/<prefix>/<blob-digest>
|- tmp/
|- restore-journal/
`- quarantine/
```

The concrete v1 local path also includes `BlobEncoding` and encoded length.
`BlobDescriptor.digest` identifies canonical uncompressed bytes, but two valid
physical encodings of those bytes can have different lengths. Retaining that
representation discriminator prevents an action record from describing one
encoded stream while the CAS serves another. Remote storage may normalize to
one representation later, but the shared store interface returns missing
`BlobDescriptor` values rather than ambiguous bare digests.

Invariants:

- blobs are immutable;
- writes use a temporary file, flush, fsync, and atomic rename/create-if-absent;
- an action record is visible only after every referenced blob is durable;
- readers verify size and digest before use;
- a corrupt local blob is moved to quarantine and may be fetched remotely;
- multiple Octa processes may read and publish without a global cache lock;
- garbage collection uses its own advisory lock;
- temporary files older than a configurable grace period are removed.

### Local retention

The local profile defines maximum bytes, high/low watermarks, and a maximum
number of filesystem entries visited by one maintenance scan. Cleanup first
expires old action records and then performs mark-and-sweep over bundles no
longer reachable from retained actions. Access timestamps are batched or sampled
to avoid a metadata write on every cache hit.

Readers that already opened a bundle are allowed to finish while garbage
collection removes its directory entry. Windows sharing failures are retried
later instead of blocking normal execution.

## Remote cache protocol

The HTTP protocol exposes the action-cache and CAS distinction:

```text
GetActionResult
FindMissingBlobs
ReadBlob
WriteBlobIfAbsent
WriteActionIfAbsent
```

The server may return a streamed response or a short-lived signed object URL.
The Octa client does not infer S3 object names or depend on S3 ETags for content
integrity.

Publication order is always:

1. Capture and verify the complete result.
2. Ask which blobs are absent.
3. Upload absent blobs.
4. Publish the action record with create-if-absent semantics.

The action record must never reference a blob that is not yet readable. This is
the same fundamental division used by the Bazel Remote Execution API, where the
Action Cache maps action digests to results and CAS stores content-addressed
blobs:
<https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto>.

For an S3-compatible backend, immutable writes can use a conditional
`If-None-Match: *` operation. The first writer succeeds and concurrent writers
do not overwrite it:
<https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html>.

### Concurrent publication and nondeterminism

If two executions produce the same action digest and equal bundle digests,
their writes are idempotent. If their bundle digests differ:

- the existing action record is not overwritten;
- the new unreferenced bundle is later collected;
- Octa emits a `nondeterministic_result` diagnostic;
- the server records a conflict metric and audit record.

Distributed action leases that prevent two agents from executing the same miss
belong to OctaCity coordination. They are not required for cache correctness and
are deliberately excluded from the first Octa implementation.

## Failure behavior

The default cache is an optimization, not a build dependency:

- lookup timeout or unavailable remote -> execute the task;
- corrupt local blob -> quarantine, try remote, then execute;
- corrupt remote blob -> reject it and execute;
- restore failure -> roll back and execute;
- upload failure -> preserve successful task completion and emit a warning;
- cancellation -> stop transfer and remove staging data;
- repeated remote failures -> open a per-run circuit breaker so every task does
  not repeat the same timeout;
- local disk full -> run garbage collection, retry once, then bypass cache.

An explicit `required` operational mode may make cache-infrastructure failures
fatal for controlled environments, but is not the default.

Remote uploads are awaited before the runner emits its terminal `Finished`
message. Upload failure does not fail the task, but background uploads must not
be silently abandoned when the runner exits.

## Security and cacheability restrictions

Version one does not cache:

- failed or cancelled tasks;
- tasks consuming secret variables or secret environments;
- tasks producing secret structured outputs;
- interactive or raw tasks;
- tasks with ignored command failures;
- dry runs;
- condition or precondition skips;
- tasks that omit an explicit `files.inputs` contract; an explicit empty list is
  valid when there are no filesystem inputs;
- tasks with special files or unsafe symlinks;
- tasks using unlocked plugins for remote writes;
- Native tasks without an environment digest;

Cache namespaces are an authorization boundary, not part of the action digest.
OctaCity must isolate organizations/projects and prevent untrusted pull requests
from writing trusted cache entries. Job credentials must be short-lived and
read/write scopes must be independently configurable.

Network-enabled tasks can be cached only because the author explicitly opted
in. Octa cannot prove that a remote response was deterministic; this limitation
must be visible in documentation and `cache explain` output.

## Runner protocol and events

Runner protocol support should be introduced as a new negotiated protocol
version rather than silently changing v1:

```rust
pub struct RunRequest {
  // Existing execution fields.
  pub cache: Option<CacheSessionSpec>,
}
```

`CacheSessionSpec` contains endpoint, token-file path, namespace, read/write
mode, environment digest, timeouts, and transfer limits. It never contains the
token value.

Runner capabilities include `task-result-cache-v1`.

Semantic events are limited to task-level operations:

```text
cache_lookup_started
cache_hit
cache_miss
cache_restore_finished
cache_publish_started
cache_published
cache_error
```

Per-blob events are intentionally omitted. `TaskResult` gains an optional
`CacheOutcome` containing action digest, hit layer (`local` or `remote`), bytes,
timings, and a machine-readable miss/error reason.

`--force` bypasses lookup but may attempt publication. If an immutable action
record already exists, Octa compares bundle digests and reports nondeterminism
rather than overwriting the old result.

## Miss diagnostics

The CLI exposes:

```console
octa cache status
octa cache explain build
octa cache prune
```

`cache explain` compares the current descriptor with the last safe local
descriptor for the logical task slot:

```text
task definition      changed
input root           unchanged
environment          unchanged
runtime image        unchanged
plugin shell         unchanged
arguments            unchanged
result               miss: task definition changed
```

Machine-readable reasons include:

- `dry_run`;
- `secret_variables`;
- `force`;
- `action_not_found`;
- `input_snapshot_failed` and `input_recheck_failed`;
- `inputs_changed_during_lookup` and `inputs_changed_before_publish`;
- `lookup_failed`, `blob_lookup_failed`, and `blob_read_failed`;
- `restore_failed` and `missing_output_bundle`;
- `output_capture_failed`;
- `blob_conflict` and `blob_publication_failed`;
- `result_metadata_invalid` and `resource_contract_invalid`;
- `deferred_failed`;
- `nondeterministic_result` and `publication_failed`.

`cache explain` may additionally compare a previous diagnostic descriptor with
the current one and report which identity component changed. Those comparison
labels are explain output, not cache lifecycle reasons stored in `CacheOutcome`.

Stored diagnostic descriptors contain digests and safe names, never secret
values or bearer credentials.

## Implementation phases

### Phase 0: ADR, threat model, and baseline

- Write the cache responsibility and trust-boundary ADR.
- Freeze the cacheable-task invariants in tests.
- Measure current freshness, startup, and non-cache task performance.
- Define action-key and output-bundle version-one formats.
- Add cross-platform golden fixture inputs.
- Set performance thresholds before implementation results are reviewed.

Completion criterion: one specification unambiguously determines what affects
an action key, what can be restored, and what is rejected as uncacheable.

### Phase 1: protocol and canonical identity

- Create `octa-cache-protocol`.
- Implement tagged `Digest`, bounded wire types, and schemas.
- Implement explicit canonical binary encoding.
- Implement `ActionDescriptorV1` and `ActionResultV1`.
- Add golden tests on Linux, macOS, and Windows.
- Assert that two absolute workspaces produce the same action digest.
- Assert that every semantic input change produces a different digest.
- Assert that ordering, absolute root, timestamps, and presentation do not.

Completion criterion: identical fixtures on two agents of the same execution
platform receive exactly the same action digest.

### Phase 2: input snapshot and output bundle

- Implement relative input-tree discovery and hashing.
- Reuse `.octaignore` semantics without rescanning for every pattern.
- Add the runtime-wide bounded hashing scheduler.
- Detect file mutation during hashing.
- Implement deterministic streaming bundle creation and Zstandard encoding.
- Implement bounded, path-safe bundle extraction.
- Add fuzz/property tests for canonical paths and the bundle decoder.

Completion criterion: a bundle created in one workspace restores byte-identical
outputs in another workspace and retains the same semantic digest.

### Phase 3: local CAS and restore transactions

- [x] Extend `octa-cache` with the single `CacheStore` boundary.
- [x] Implement concurrent immutable local blob writes.
- [x] Implement atomic action-record publication.
- [x] Implement canonical current-output inspection shared with bundle packing, so
  an already materialized hit avoids rewriting identical files.
- [x] Implement restore staging, journal, rollback, and startup recovery.
- [x] Implement corruption quarantine.
- [x] Implement capacity limits and garbage collection.
- [x] Run multi-process reader/writer and fault-injection tests.

Completion criterion: terminating the process at every durable write/restore
stage cannot expose an incomplete action or leave live outputs partially
replaced.

### Phase 4: Octafile, planner, and executor

- [x] Add and validate the task `files` contract and non-duplicating `cache` block.
- [x] Remove `sources`, `output`, `source_strategy`, the source-strategy registry,
  legacy path hashing, freshness state, freshness DAG actions, and the sled
  fingerprint database without a compatibility translation.
- [x] Make watch mode consume `files.inputs` through the shared `InputSnapshotter`.
- [x] Rename the execution-local `run: once/changed` storage to invocation reuse and
  keep its control-flow semantics independent from persistent result caching.
- [x] Add task-level `CacheLookup` and `CacheFinalize` DAG actions.
- [x] Resolve conditions, preconditions, dependencies, variables, and environment
  in the defined order.
- [x] Restore filesystem, stdout, public outputs, artifacts, and reports on a hit.
- [x] Capture and publish only after full successful completion.
- [x] Recheck inputs before publication.
- [x] Add cache outcome to structured task results.
- [x] Add cache lifecycle events without per-blob noise.

Completion criterion: after deleting outputs, a second run restores them and
does not invoke the task command; unchanged materialized outputs are not
rewritten; watch and action identity observe the same input set.

### Phase 5: runtime, CLI, and runner integration

- [x] Add cache profile loading to `octa-runtime`.
- [x] Keep state directory and result-cache configuration separate.
- [x] Compose the local store at CLI and runner roots; remote layering remains
  part of Phase 8.
- [x] Add `cache status`, `cache explain`, and `cache prune`.
- [x] Introduce the next runner protocol version and schema.
- [x] Add cache capability discovery and token-file validation.
- [x] Add environment identity to headless execution.
- [x] Extend runner end-to-end fixtures.

Completion criterion: CLI and runner use the same cache engine, compute the
same action digest, and report the same result semantics.

### Phase 6: plugin-provided filesystem contracts

- Replace the current plugin protocol with a version that supports a bounded,
  side-effect-free planning request before cache lookup; do not retain a
  compatibility adapter for the previous protocol.
- Add a `PluginCachePlan` containing required input patterns and exact output
  roots. Planning may depend on validated task parameters, working directory,
  target platform, and plugin identity, but must not execute the task or mutate
  the workspace.
- Resolve every plugin step before computing the action identity and combine
  its required contract with user-declared `files.inputs` and `files.outputs`.
  User declarations add to the contract and cannot remove a plugin-required
  input or output.
- Permit `files` to be omitted when every executable step supplies a complete
  plugin contract. Shell and other opaque commands still require explicit
  inputs when task-result caching is enabled; never interpret an unknown
  contract as an empty one.
- Validate plugin-provided paths with the existing `octa-cache` rules: bounded
  portable relative paths, no workspace escape, no overlapping output roots,
  and no input/output overlap.
- Include the canonical effective contract in the resolved task-definition
  identity so a plugin planning change cannot reuse an older action result.
- Keep artifact and report registration independent from cache ownership. A
  registered resource may point inside a cached output root, but registration
  alone does not make an arbitrary path restorable.
- Add protocol, planner, mixed shell/plugin, invalid-path, action-key, and
  end-to-end restore tests. Include an official plugin fixture that caches a
  task without a user-authored `files` block.

Completion criterion: a specialized plugin task without `files` computes the
same effective contract in CLI and runner, restores its declared result on a
hit, and changes its action digest whenever the plugin plan changes. An opaque
shell task cannot accidentally become cacheable with an empty input identity.

### Phase 7: distributed tool caches

- Model tool caches separately from task-result caching. A task-result hit
  skips execution; a tool-cache hit only accelerates one operation inside a
  task, and deleting tool-cache state must never change build correctness.
- Extend the plugin plan with bounded opaque tool-cache requirements containing
  an adapter identifier, isolation scope, and sharing mode (`shared`, `locked`,
  or `private`). Do not add compiler-, language-, or BuildKit-specific variants
  to Octa Core.
- Add a versioned `ToolCacheSession` to the runner/plugin boundary with a local
  L1 directory, optional remote L2 endpoint, namespace, read/write policy, and
  a path to short-lived scoped credentials. Credentials never enter command
  arguments, action identities, events, or logs.
- Let the concrete plugin adapter own fine-grained key construction and native
  tool configuration. Octa supplies lifecycle, policy, isolation, and
  observability; it does not attempt to reproduce compiler or BuildKit key
  semantics.
- Publish immutable objects independently with digest verification and atomic
  create-if-absent semantics so another agent can consume completed compilation
  units before the producer task finishes.
- Never archive a mutable compiler, package-manager, or BuildKit working
  directory as an action-result output bundle. Local hot state and remote
  content-addressed objects have independent quotas, retention, and garbage
  collection.
- Add `ToolCacheHit`, `ToolCacheMiss`, `ToolCacheDownload`,
  `ToolCacheUpload`, and `ToolCacheError` events with adapter, layer, bytes, and
  duration fields, while avoiding per-object log noise at normal verbosity.
- Implement the first official compiler-cache adapter, followed by BuildKit.
  Test concurrent equal publication, corrupt downloads, unavailable remote
  storage, namespace isolation, cancellation, and secret redaction.

Completion criterion: two runner processes with separate workspaces and local
cache directories can use one remote tool-cache service; the second build still
executes but reuses verified fine-grained results produced by the first. Tool
cache failure degrades to normal execution without changing the task result.

### Phase 8: remote HTTP client

- Create `octa-cache-http`.
- Implement persistent connections and streaming transfers.
- Implement batch missing-blob lookup.
- Implement create-if-absent writes.
- Implement bounded retries with jitter and server retry hints.
- Implement per-operation deadlines and cancellation.
- Implement a per-run circuit breaker.
- Implement layered local-first lookup and local population after a remote hit.
- Add an in-memory reference server used only by protocol contract tests.

Completion criterion: two runner processes with different workspaces share a
result through HTTP without a shared filesystem.

### Phase 9: correctness and security hardening

- Test truncated, oversized, and corrupt manifests/bundles.
- Test path traversal, symlink escape, special files, and decompression bombs.
- Inject crashes at every local publication and restore transition.
- Test cancellation during hashing, download, extraction, packing, and upload.
- Test concurrent equal and conflicting action publications.
- Test mutation between pre-execution and post-execution input snapshots.
- Test that secrets never enter keys, metadata, events, bundles, or logs.
- Test HTTP authentication and `401`, `403`, `404`, `409`, `412`, `429`, and
  `5xx` behavior.
- Run local and HTTP implementations through one store contract suite.

Completion criterion: a cache failure cannot turn into a false successful build,
leak a secret, escape the workspace, or leave partial outputs.

### Phase 10: performance validation

Extend `benchmarks/agent-ready` with:

- 1, 1,000, and 100,000 small input files;
- one 1-5 GiB input;
- 10,000 small output files;
- large compressible and incompressible outputs;
- cold local miss;
- warm local hit;
- remote hit over loopback;
- remote hit under controlled 20 ms and 80 ms latency;
- cold and warm compiler-tool-cache builds;
- concurrent agents consuming objects while another build publishes them;
- 25 parallel cacheable tasks;
- overlapping input trees;
- two different absolute workspaces;
- current Octa baseline and equivalent go-task scenarios.

Record:

- wall-clock time and p95;
- hashing throughput and CPU time;
- peak RSS;
- filesystem operation count;
- bytes uploaded and downloaded;
- HTTP request count;
- tool-cache object hit rate and time saved inside executed tasks;
- compression ratio;
- cache hit layer and miss reason.

Predefined acceptance rules:

- tasks without a `cache` block regress by no more than 5%;
- one runtime-wide hashing budget prevents CPU-pool multiplication;
- a cache miss has bounded overhead and no redundant second filesystem walk;
- a warm local hit remains within the predeclared comparison bound against the
  recorded legacy-freshness baseline and go-task;
- a second absolute workspace always hits for an equivalent action;
- remote transfer time is dominated by bundle bytes and network conditions,
  not thousands of small-object requests;
- enabling automatic plugin planning has bounded startup cost, and a warm
  compiler-tool-cache build improves the recorded uncached baseline;
- no optimization replaces content identity with timestamp-only identity.

Completion criterion: release benchmarks pass the fixed regression thresholds
on a developer machine and the stable Linux agent class, with raw samples and
environment metadata retained.

## Required test matrix

The implementation is not complete until all of the following are covered:

- Linux, macOS, and Windows canonical-identity fixtures;
- same content under different absolute roots;
- every action component independently causing a miss;
- irrelevant ordering and presentation changes preserving a hit;
- missing and tampered outputs being fully replaced;
- cached structured dependency values matching executed values;
- artifacts and reports remaining valid after restore;
- plugin-provided and user-added file contracts producing one canonical action
  identity across CLI and runner;
- opaque commands being rejected instead of receiving an implicit empty input
  contract;
- distributed tool-cache namespace isolation, concurrent publication,
  unavailable-service fallback, and corrupt-object rejection;
- watch and action snapshots selecting the same files for the same contract;
- removed `sources`, `output`, and `source_strategy` fields being rejected by
  schema validation rather than silently translated;
- concurrent reads, writes, restore, and garbage collection;
- forced process termination and subsequent recovery;
- remote timeouts and transient/permanent HTTP errors;
- corrupt local followed by valid remote fallback;
- corrupt remote followed by local execution;
- source mutation suppressing publication;
- secret-bearing invocations bypassing cache and secret structured outputs
  being rejected from cacheable tasks;
- deterministic and nondeterministic duplicate publication;
- cancellation leaving no live temporary or partial output state;
- protocol/schema/golden-document agreement;
- output decoder fuzzing and path normalization property tests.

## Explicit non-goals for the Octa phase

- direct S3 access from Octa;
- agent-to-agent transfer;
- remote execution;
- caching an entire workspace;
- caching failed task results;
- parsing arbitrary shell text to infer toolchain dependencies;
- trusting S3 ETag as the content digest;
- background upload after runner completion;
- distributed action leases;
- metadata-only source identity;
- a generic provider/plugin system for cache internals.

## Definition of done

The Octa cache work is complete when:

1. A task result created in one workspace is restored in a different workspace.
2. The command is not invoked on a valid hit.
3. Files, structured outputs, artifacts, and reports have identical logical
   results after restore.
4. Action identity is stable across supported hosts within its declared
   platform scope.
5. All key-relevant changes produce misses and all known irrelevant changes do
   not.
6. Corruption, network failure, cancellation, and process crashes fail closed
   and preserve workspace integrity.
7. Cache miss reasons are inspectable without secret disclosure.
8. CLI and runner share one implementation and a versioned protocol.
9. A reference HTTP service proves cross-process, cross-workspace reuse.
10. The complete correctness, security, coverage, and performance gates pass.
11. Legacy freshness code and storage are gone, and watch and cache identity use
    the same production file selector and snapshotter.

After this point OctaCity can implement the production Action Cache API,
S3-compatible storage, authorization, retention, agent-local L1 behavior, and
eventual distributed action leases without duplicating or overriding Octa task
semantics.
