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

Omitted optional values use the centralized defaults in `octa-cache`. Unknown
fields, zero resource bounds, inconsistent watermarks, and unsupported
compression settings fail before task execution.

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

Remote endpoints are intentionally absent from the CLI profile in this phase.
They will be implemented by the versioned HTTP cache client; Octa will not put
an S3 SDK or server storage details into this profile.
