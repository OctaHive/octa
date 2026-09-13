# Task-result cache profile

The filesystem contract belongs in `Octafile.yml`, but storage paths, capacity,
toolchain identity, and resource limits depend on the machine running Octa.
They are loaded from a separate strict TOML profile:

```toml
mode = "read_write"
namespace = "project/example"

[local]
directory = "/var/cache/octa"
max_bytes = 21474836480
high_watermark_bytes = 19327352832
low_watermark_bytes = 17179869184

[environment]
identity = "linux-amd64-rust-1.98-toolchain-v1"

# Optional shared L2. The local cache above remains the verified L1.
[remote]
endpoint = "https://cache.example.com/tenant-a/"
token_file = "/run/secrets/octa-cache-token"
# Optional PEM root for a private certificate authority.
# ca_certificate_file = "/etc/octa/cache-ca.pem"
request_timeout_seconds = 30
max_parallel_transfers = 8
```

Run a cacheable task with:

```console
octa --cache-profile cache.toml build
```

`OCTA_CACHE_PROFILE` provides the same setting. A relative profile path is
resolved from the selected workspace. `local.directory` must be an absolute,
operator-owned directory outside repository workspaces; repositories and task
processes must not be allowed to replace its layout directories with links or
junctions. The normal `OCTA_DATA_DIR` remains separate: it stores monorepo discovery metadata,
whereas `local.directory` contains the versioned action cache, CAS, restore
journals, and quarantine.

## Required fields

- `mode`: `read_only`, `write_only`, or `read_write`;
- `namespace`: logical authorization namespace used for action records;
- `local.directory`: local CAS parent directory;
- `environment.identity`: operator-controlled identity of the complete native
  toolchain and relevant host configuration.

The environment string is hashed with BLAKE3 before entering an action key. It
must change whenever compiler versions, SDKs, system libraries, or other
undeclared host tools can change task results. Absolute workspace paths are not
part of the identity, so equivalent workspaces can reuse one result.

## Optional limits

`[local]` accepts `max_expanded_blob_bytes`,
`max_blob_compression_ratio`, `max_entries`, `temporary_grace_seconds`, and
`access_update_interval_seconds` in addition to the capacity watermarks shown
above.

`[snapshot]` accepts `max_parallel_hashes`, `max_entries`,
`read_buffer_bytes`, and `mutation_retries`.

`[bundle]` accepts:

```toml
[bundle]
compression = "zstd" # or "identity"
compression_level = 3
max_encoded_bytes = 21474836480
max_expanded_bytes = 107374182400
max_entries = 1000000
max_path_bytes = 16384
max_file_bytes = 21474836480
max_compression_ratio = 1000
read_buffer_bytes = 1048576
```

Omitted optional values use the centralized defaults in the cache crates. Unknown
fields, zero resource bounds, inconsistent watermarks, and unsupported
compression settings fail before task execution.

`[remote]` enables the version-one HTTP action cache and CAS described in
[`cache-http-v1.md`](cache-http-v1.md). `endpoint` and `token_file` are
required; `ca_certificate_file` optionally adds one PEM root for a private PKI.
The endpoint must use HTTPS and contain no credentials, query, or fragment.
Credential and certificate paths must be absolute. On Unix the regular, non-symlink token
file must be owned by the effective process user and must not grant group or
other permissions. On Windows the service installer is responsible for
restricting the file ACL to the agent account. Its value is never serialized or
logged.

In addition to the timeout and transfer limit shown above, operators may set
`max_retries`, `retry_base_delay_milliseconds`,
`retry_max_delay_milliseconds`, `circuit_failure_threshold`, and
`circuit_open_seconds`. Remote failures fall back to task execution or
local-only publication; cancellation still cancels the task. A remote action
is fetched without its blob, checked against job limits, then populated into
the local CAS only as part of a verified restore.

## Inspection and maintenance

```console
octa --cache-profile cache.toml cache status
octa --cache-profile cache.toml cache explain build
octa --cache-profile cache.toml cache prune
```

`status` reports the versioned local layout and capacity. `explain` computes the
same action identity and probes the same store as execution, but never invokes
task bodies, restores outputs, or publishes results. It reports every cacheable
task reached by the selected graph. Computing the exact identity does resolve
the task's dynamic context: required variables, secret providers, and
plugin-backed template values may therefore perform their documented reads or
external calls. `prune` performs an exclusive bounded
mark-and-sweep pass and reports removed action, blob, and maintenance objects.

`status` and `prune` remain local maintenance operations. The remote service
owns its own quota, retention, and backing storage; Octa does not expose S3
details or use an S3 SDK.
