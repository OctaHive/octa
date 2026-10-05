# Codex plugin compatibility

The Codex task plugin executes only the binary selected by the local operator
through the `OCTA_CODEX_EXECUTABLE` environment variable. The value must be an
absolute path to a directly executable native file; the plugin never searches
`PATH` and never invokes a shell as a fallback.

The currently supported Codex CLI compatibility set is:

- `0.130.0`

Each listed release is covered by the plugin's machine-interface fixtures. The
plugin runs a bounded `codex --version` probe before reading task files and
rejects any other release. The selected file is fingerprinted before the probe,
then checked again after it, so compatibility evidence is retained together
with the exact file identity rather than a path alone. The task-process phase
will revalidate that identity at its spawn boundary.

Installation, authentication, and protecting the selected path from writes by
job workloads remain operator responsibilities. Broader task configuration,
secret handling, records, cancellation, and examples will be documented when
their corresponding implementation phases are complete.
