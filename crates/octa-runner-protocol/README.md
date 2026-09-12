# octa-runner-protocol

`octa-runner-protocol` contains the versioned JSON Lines wire contract shared
by `octa-runner` and external process supervisors.

The crate exposes input and output DTOs, protocol constants, the input frame limit, and
the corresponding JSON Schemas. It intentionally contains no Octa execution
logic or async transport implementation.

The crate follows SemVer. Wire compatibility is tracked separately by
`RUNNER_PROTOCOL_VERSION`; incompatible wire changes require a new protocol
version.

Protocol v2 adds an optional, job-scoped `CacheSessionSpec` containing local
CAS placement, access mode, namespace, and an exact Native or OCI runtime
identity. A remote session contains only an HTTPS endpoint and token-file path,
never bearer token contents. See the repository
[runner protocol documentation](../../docs/runner-protocol.md) for lifecycle
and capability negotiation.
