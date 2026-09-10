# octa-runner-protocol

`octa-runner-protocol` contains the versioned JSON Lines wire contract shared
by `octa-runner` and external process supervisors.

The crate exposes input and output DTOs, protocol constants, the input frame limit, and
the corresponding JSON Schemas. It intentionally contains no Octa execution
logic or async transport implementation.

The crate follows SemVer. Wire compatibility is tracked separately by
`RUNNER_PROTOCOL_VERSION`; incompatible wire changes require a new protocol
version.
