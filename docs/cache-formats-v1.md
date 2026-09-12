# Octa cache canonical formats version 1

This document fixes the byte-level formats used by `octa-cache`. Integers are
unsigned and big-endian. Strings are UTF-8 and contain no terminator. Paths use
`/`, are relative to the workspace, and satisfy `RelativePath` validation.
Changing any encoding below requires a new format version and new golden
fixtures.

## Action key

The action key answers whether one successful task execution can safely replace
another. It is BLAKE3 over the domain-separated `octa.action.v1` stream defined
by `ActionDescriptorV1`. The stream contains, in fixed tagged order:

- the action-key format version;
- digests of the resolved task definition and canonical input tree;
- the workspace-relative working directory;
- digests of resolved public variables and the selected environment;
- OS, architecture, and either the native toolchain/environment digest or the
  immutable OCI image digest;
- uniquely named plugin identities, sorted by name, including version,
  protocol version, and executable SHA-256;
- command arguments in semantic order, timeout, and optional user salt.

Every integer is big-endian. Strings are length-prefixed, collections include
their count, digests retain their algorithm and source byte size, runtime
variants have distinct discriminants, and optional values have explicit
presence bytes. Thus neither concatenation ambiguity nor producer-specific map
ordering can create an accidental collision before BLAKE3 is applied.

Absolute workspace paths, timestamps, agent/job IDs, storage configuration,
credentials, and presentation settings are deliberately excluded: including
them would prevent equivalent work from sharing results. Executor behavior that
affects task semantics is instead represented by an explicit semantic epoch in
the task-definition digest. That epoch is independent of the Octa release
version and is incremented only for an incompatible execution-semantics change.
Changing the descriptor fields, their normalization, tags, or byte encoding
requires a new action-key format and domain.

## Action result

`ActionResultV1` is bounded JSON metadata stored under an action key. It repeats
the action digest to detect a record fetched from the wrong key or namespace and
references an optional immutable output bundle. It also carries only the
observable logical state needed after a hit: bounded stdout used as a dependency
result, public structured outputs, and plugin-defined artifact/report
registrations. Failed or cancelled executions, secrets, full logs, timing, and
agent identity are not cache results.

Publication stores the content-addressed bundle before making the action record
visible. Restore validates the record and output contract, verifies the decoded
bundle digest in staging, commits declared outputs atomically, and only then
replays logical outputs and registrations. The result version is independent of
the action-key and bundle versions because those formats can evolve for
different reasons.

## Input tree

The input-tree digest is BLAKE3 over this stream:

```text
u32 domain_length
bytes "octa.input-tree.v1"
u64 entry_count
repeated entry_count times, sorted by relative path bytes:
  u8 entry_tag                 # directory=1, file=2, symlink=3
  u32 path_length
  bytes path
  if file:
    u8 executable              # exactly 0 or 1
    u8 digest_algorithm        # blake3=1, sha256=2
    u64 content_size
    bytes[32] content_digest
  if symlink:
    u32 target_length
    bytes target
```

The `Digest.size_bytes` for an input tree is the length of this canonical
metadata stream, not the sum of file sizes. File contents are represented by
their tagged content digests. Absolute workspace paths, timestamps, ownership,
and presentation settings never enter the stream.

## Output bundle

An uncompressed output bundle is this stream:

```text
bytes[8] "OCTABND1"
repeated BlobDescriptor.entry_count times, in canonical tree order:
  u8 entry_tag                 # directory=1, file=2, symlink=3
  u32 path_length
  bytes path
  if file:
    u8 executable              # exactly 0 or 1
    u64 content_length
    bytes content
  if symlink:
    u8 target_is_directory     # exactly 0 or 1; needed by Windows
    u32 target_length
    bytes target
```

The bundle digest and expanded size cover this complete uncompressed stream.
`identity` stores it directly. `zstd_v1` encodes the same stream as one
Zstandard frame whose back-reference window is at most `2^27` bytes (128 MiB);
its compression level is operational configuration and does not affect content
identity. Producers constrain the window and consumers reject a larger request
before decoding payload bytes.

Decoders must enforce window size, encoded size, expanded size, entry count,
individual file size, path length, and compression-ratio limits before trusting
a result.
Canonical tree order compares individual UTF-8 path components bytewise,
places an ancestor before its children, and keeps each directory subtree
contiguous. Every descendant has an explicit directory entry for its immediate
parent; only parents above a declared output root are implicit. Absolute paths,
traversal, special files, unsafe symlink targets, trailing canonical bytes, and
entries outside the declared output roots are rejected.

## Golden values

The cross-workspace input fixture in `octa-cache` has this digest:

```text
blake3:31b55def498c0ba8e6f838626e7746ab8ad709d1932f132f354ebd358a66f946:110
```

The output fixture containing `out/`, two non-executable files, and an empty
directory has this semantic bundle digest:

```text
blake3:eba1855e80c84ed8767f4612ca90be47d791fd7cc6f8f1496f242efaca6eecd8:85
```

Tests create both fixtures under fresh absolute roots. Linux, macOS, and
Windows must produce these exact values.
