# Codex task plugin

The official `codex` plugin runs one unattended Codex CLI invocation as an
ordinary Octa task. Octa still owns the DAG, timeouts, variables, secret
resolution, output validation, and resource registration. The plugin owns only
the Codex process tree, machine-readable event stream, and sanitized audit
records.

There is no OctaCity client in Octa or in this plugin. Locally, `octa` starts
the plugin directly. In distributed execution, an OctaCity agent prepares the
workspace and environment, starts `octa-runner`, and later collects the same
generic artifacts, reports, outputs, and events as for any other task.

## Installation and compatibility

Octa release archives include `octa_plugin_codex`, its platform manifest, and
the entry in `Octa.lock`. They do **not** include Codex CLI or authentication.
The machine or agent image operator must:

1. install a supported native Codex CLI executable;
2. protect it from modification by job workloads;
3. set `OCTA_CODEX_EXECUTABLE` to its absolute path in the plugin process
   environment; and
4. supply authentication through an environment-specific Octa secrets
   profile.

Tasks that opt into blocking tool authorization additionally require the
operator to set `OCTA_CODEX_TOOL_AUTHORIZER` to an absolute protected native
helper. The helper receives the bounded Codex `PreToolUse` document on stdin
and returns a supported Codex hook disposition on stdout. A task cannot select
or replace this executable.

The plugin never searches `PATH` and never invokes a shell as a fallback. It
runs a bounded `--version` probe and fingerprints the selected executable
before accepting work, then revalidates that identity at process spawn.

The currently supported Codex CLI compatibility set is:

- `0.161.0`

Each listed release is covered by the plugin's machine-interface fixtures.
Any other release is rejected before prompt files, credentials, or workspace
records are read or created.

## Configuration

A `codex` value is a strict object: unknown fields are rejected. Exactly one
of `prompt` and `prompt_file` is required. All paths are portable,
workspace-relative paths using `/`; absolute paths, traversal, ambiguous
Windows names, and unsafe separators are rejected.

| Field | Meaning |
| --- | --- |
| `prompt` | Non-empty inline prompt, limited to 1 MiB of UTF-8. |
| `prompt_file` | File containing the prompt, relative to the effective task directory and limited to 1 MiB. |
| `model` | Optional model name passed through the typed Codex invocation. |
| `reasoning_effort` | Optional `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. |
| `result_schema` | Optional JSON Schema. When present, Codex must produce a structured value satisfying it; prose is never parsed as a substitute. |
| `run_records` | Root for audit records; defaults to `.octa/codex-runs`. |
| `environment` | Explicit `public` and `secret` child-environment mappings from environment names to Octa variable names. |
| `source_revision` | Optional bounded revision identifier copied into provenance. |
| `deliverables` | At most 64 exact required artifacts or reports to register after a successful, validated run. |
| `tool_authorization` | Optional `required`; fails before prompt or credentials are exposed unless the operator-selected blocking authorizer is available. |

<!-- codex-config -->
```yaml
codex:
  prompt: Review the workspace and return a concise summary.
  model: gpt-codex
  reasoning_effort: high
  environment:
    public:
      CI: CI_MODE
    secret:
      OPENAI_API_KEY: CODEX_AUTH
  source_revision: 8de7f1c
```

<!-- codex-config -->
```yaml
codex:
  prompt_file: prompts/review.md
  result_schema:
    type: object
    properties:
      verdict:
        type: string
        enum: [approved, changes_required]
    required: [verdict]
    additionalProperties: false
  run_records: .octa/codex-audit
  tool_authorization: required
  deliverables:
    - kind: artifact
      name: proposed-patch
      path: out/change.patch
      content_type: text/x-diff
    - kind: report
      name: review-result
      path: out/review.json
      format: codex.review.v1
```

`environment.public` and `environment.secret` map a child environment name
to an already resolved Octa variable name; they never contain the value. A
secret mapping is accepted only when the source variable is marked secret by
Octa. An unrelated variable, including another secret, is not inherited by the
Codex process.

`tool_authorization: required` installs one synchronous `PreToolUse` hook for
all Codex-supported local tool paths and enables it only for that invocation.
The helper executable is fingerprinted before task material is read and again
immediately before Codex starts. Hook configuration is supplied as a fixed
command-line override, so repository files cannot replace it. The operator
must protect both Codex and the helper from workload mutation after spawn.
Because Codex hooks are a guardrail rather than the final security boundary,
the outer Agent/backend must independently enforce the same disposition at
the protected operation.

## Minimal child environment and authentication

The child starts from an empty environment. When present in the effective task
environment, the plugin may retain only this platform baseline:

- Unix: `HOME`, `PATH`, `TMPDIR`, `LANG`, `LC_ALL`, `SSL_CERT_FILE`, and
  `SSL_CERT_DIR`.
- Windows: `SystemRoot`, `WINDIR`, `ComSpec`, `PATH`, `PATHEXT`, `TEMP`,
  `TMP`, `USERPROFILE`, `APPDATA`, and `LOCALAPPDATA`.

All other values require an explicit mapping. If a baseline value contains a
resolved secret, it is rejected until it is supplied through
`environment.secret`.

Authentication belongs in an Octa secrets profile selected by the local
operator or agent, not in the Octafile or runner request. For example, a task
can refer to a secret variable named `CODEX_AUTH`, while the active profile
resolves that name from Vault, an exec provider, or a protected file. The task
then maps `OPENAI_API_KEY: CODEX_AUTH` under `environment.secret`. The value is
passed only to the selected child process and is redacted before plugin output,
diagnostics, or records are retained.

Exact-value redaction cannot recognize arbitrary encoded or transformed
credentials. Use narrowly scoped, short-lived credentials and treat the outer
agent sandbox and secret policy as the primary security boundary.

## Results and audit records

A well-formed terminal Codex event completes the plugin command with code zero
even when its semantic `outcome` is `blocked`, `needs_input`,
`budget_exhausted`, or `failed`. Workflows that require `completed` must gate
on the exported `outcome`; semantic failure is data, while an invalid or
unauditable execution is a task failure.

Each normalized run atomically publishes a unique directory below
`run_records` with these version-one records:

- `records/trace.jsonl` — ordered sanitized events, each carrying
  `format_version`, `sequence`, and `event`;
- `records/result.json` — semantic outcome, the final message or validated
  structured result, harness identifiers, and usage counters; and
- `records/provenance.json` — plugin and observed Codex versions, non-secret
  model settings, BLAKE3 prompt digest, optional source revision, timing,
  outcome, and usage.

The trace and provenance are registered as `codex-run-trace` and
`codex-run-provenance` artifacts. The result is registered as the
`codex-run-result` report with format `octa.codex.result.v1`. Records never
contain the full prompt, a complete environment dump, or selected secret
values. Declared deliverables are revalidated only after the process tree has
stopped and use Octa's existing artifact/report protocol; the plugin never
uploads files itself.

## Cache and cancellation

The plugin deliberately provides no automatic cache plan. Model responses,
remote services, credentials, and tool state are not a complete deterministic
filesystem contract. An author may opt in with an explicit complete Octa cache
configuration, but reuse can be unsound even when workspace inputs have not
changed.

Octa's task timeout and cancellation token are authoritative. Cancellation
stops prompt delivery, requests graceful termination of the complete Unix
process group or Windows Job Object, waits up to two seconds, and then
force-terminates remaining descendants. Pipe draining and force termination
are independently bounded. Cancellation emits one terminal plugin response,
does not publish partial run records, and cannot leave Codex descendants
running.

## Examples

The executable [local and agent-oriented Octafiles](../example/codex/README.md)
show inline and file prompts, structured results, exact deliverables, secret
references, and a dependent semantic-outcome gate. Repository conformance runs
those same files against the deterministic Codex fixture through both the CLI
and the public runner protocol; the examples do not have a separate test-only
execution path.
