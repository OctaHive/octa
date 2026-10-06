# Codex task examples

These examples use the ordinary Octa plugin path. The Codex plugin never calls
OctaCity: locally, Octa starts it directly; on an agent, `octa-runner` starts
the same task after the agent has prepared the workspace, plugins, environment,
and secrets profile.

## Local review

[`local/Octafile.yml`](local/Octafile.yml) uses an inline prompt, declares a
review artifact, exports the semantic outcome, and gates a dependent task on
that outcome.

Install a supported Codex CLI, point the plugin at its absolute path, and run:

```console
OCTA_CODEX_EXECUTABLE=/absolute/path/to/codex \
  octa --config octa-config.yml verify-review
```

Run the command from the `local` directory. Authentication is supplied by the
operator's Codex installation; it is not stored in this example.

## Agent implementation

[`agent/Octafile.yml`](agent/Octafile.yml) reads its prompt from a file,
requires a schema-validated result, declares an artifact and a report, and
gates the dependent task on the exported semantic outcome.

The logical `CODEX_AUTH` variable comes from a secret provider named
`authentication`. An OctaCity agent supplies that provider in its selected
secrets profile and exposes the credential to the Codex child only as
`OPENAI_API_KEY`. The profile, credential value, compatible Codex executable,
and plugin lock remain deployment configuration and do not belong in the
Octafile or runner request.

Both examples deliberately omit task-result caching. Model and external-tool
state are not a complete deterministic cache contract.
