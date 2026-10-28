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
of `prompt` and `prompt_file` is required. Prompt-file, run-record and
deliverable paths are portable, workspace-relative paths using `/`; absolute
paths, traversal, ambiguous Windows names, and unsafe separators are rejected.
Operator-qualified stdio MCP executables instead use absolute paths.

| Field | Meaning |
| --- | --- |
| `prompt` | Non-empty inline prompt, limited to 1 MiB of UTF-8. |
| `prompt_file` | File containing the prompt, relative to the effective task directory and limited to 1 MiB. |
| `model` | Optional model name passed through the typed Codex invocation. |
| `reasoning_effort` | Optional `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. |
| `cli_arguments` | Ordered optional flags: `--ephemeral`, `--skip-git-repo-check`, and one `--color=never`, `--color=always` or `--color=auto`. Other flags and duplicate/conflicting values are rejected. |
| `mcp_servers` | Optional explicit named MCP connections. HTTP uses `transport: http`, `url` and optional `bearer_token_env_var` from `environment.secret`. Stdio uses `transport: stdio`, an absolute `command`, ordered `args` and explicitly mapped `env_vars`. Both support an optional `enabled_tools` list. Connections are required. An explicit map disables user-config loading; the caller must isolate other configuration layers. Omission preserves legacy behavior. |
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
  cli_arguments: [--ephemeral, --color=never]
  environment:
    public:
      CI: CI_MODE
    secret:
      OPENAI_API_KEY: CODEX_AUTH
      TICKET_TOKEN: TICKET_CREDENTIAL
  mcp_servers:
    tickets:
      transport: http
      url: https://tickets.example/mcp
      bearer_token_env_var: TICKET_TOKEN
      enabled_tools: [read_issue]
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

`cli_arguments` is a finite extension of the fixed unattended command. It cannot
replace model, schema, sandbox, approval, hooks, prompt or workspace settings, and
it is passed as argv without a shell. MCP settings are typed and bounded to 64
servers and 64 KiB of encoded configuration. Each selected server is required;
startup failure must fail the invocation rather than silently removing a tool.
An explicit map adds `--ignore-user-config`, which skips the user config while
preserving authentication lookup. Codex still merges MCP tables from other
configuration layers: `{}` does not clear those layers, and same-name settings
can retain fields from them. For exact server/tool selection, the caller must
exclude all ambient configuration sources, including project, system and
authenticated cloud-managed MCP settings. Filesystem isolation addresses local
sources only. In OctaCity, this qualification belongs to the existing
Agent/backend isolation boundary. The plugin does not inspect or rewrite host
configuration. Omitting `mcp_servers` preserves existing tasks' configuration
behavior.
HTTP authentication requires HTTPS and a name in `environment.secret`. Stdio
commands use an explicit absolute executable and forward additional variables
only through `env_vars` names already declared in `environment.public` or
`environment.secret`. `enabled_tools` restricts the selected server's tools;
an empty list exposes none. MCP configuration and credentials are not copied into
run records. The outer operator/Agent still qualifies executables, endpoints,
network and tool authority; selecting settings cannot grant that authority.

Managed external integrations use these same MCP connections or the existing
environment mappings. Octa receives prepared runtime settings, not OctaCity
Project, Role/Profile or connection-registry identities. Connection resolution,
access checks and credential ownership stay with the caller. See the official
[Codex MCP settings](https://developers.openai.com/codex/mcp/) for the underlying
transport and environment-name semantics.

### Model discovery

The plugin advertises `model-catalog.v1`. A host can explicitly request
`DescribeModels` through the existing plugin connection, or run the same
implementation once without a socket:

```sh
OCTA_CODEX_EXECUTABLE=/absolute/path/to/codex ./plugins/octa_plugin_codex --describe-models
```

Discovery uses the operator-selected qualified Codex executable and its
installation-owned access. It performs `initialize` and `model/list` through
[Codex app-server](https://learn.chatgpt.com/docs/app-server), without a thread,
inference turn, task prompt, task environment or Octa run records. Pages,
response bytes and process lifetime are bounded by the common process owner.
The single 10-second discovery deadline includes executable qualification and
app-server queries. Timeout drops the existing process owner and stderr drain;
it returns unavailable metadata without leaving the runtime running.
The result contains `available`, `manual_model`, advisory `models` with opaque
`id`/`label` values, and JSON Schema `parameters` at catalog and model level.
A missing or unavailable runtime returns `available: false` with the plugin's
manual-entry parameter contract; it never invents model identifiers.

Suggestions are not an entitlement check or an exhaustive allowlist. Keep a
manually entered identifier unchanged and let execution report unsupported
models or settings without substitution. Explicit refresh must not change a
previously saved invocation. Reads of a host's cached catalog need not start
this command. Credentials and runtime configuration remain installation-owned;
model metadata grants no execution, sandbox, tool or credential authority.

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
`run_records` with these records:

- `records/trace.jsonl` — ordered sanitized events, each carrying
  `format_version: 2`, `sequence`, `observed_at_unix_millis`, and the native
  `event` (including MCP call identifiers, server/tool, arguments and consumed
  result/error when emitted by Codex);
- `records/result.json` (version 1) — semantic outcome, the final message or validated
  structured result, harness identifiers, and usage counters; and
- `records/provenance.json` (version 2) — plugin and observed Codex versions, non-secret
  model settings, BLAKE3 prompt digest, optional source revision, timing,
  outcome, usage and `capture` metadata.

The `codex.native-mcp-evidence.v1` capability declares this capture contract.
`capture` records `status` (`complete`, `cancelled` or `failed`),
`coverage: emitted_native_events`, `terminal_observed`, `record_count`, and the
BLAKE3 digest of the exact trace bytes. Event observation times use a monotonic
clock anchored to the run's start time and stay inside its provenance window;
they do not assert remote server time. The existing 16 MiB trace limit and
bounded native framing still apply. Consumers identify exact content through
the ordinary Artifact digest plus record sequence and field path.

Cancellation, timeout and native execution/normalization failure atomically
retain the sanitized trace and provenance collected so far. They register those
two Artifacts through the existing resource protocol, including on failed task
completion, with no result report, deliverables or semantic outputs. Unknown
usage stays empty. A trace can be empty when no valid native event arrived; a
started MCP call without its completion has an unknown remote outcome.

Successful empty tool content, tool refusal (`status: failed` with consumed
result content), and a transport/protocol failure (`error` without a result)
remain distinct native observations. Capture coverage describes only events
Codex emitted: it cannot certify complete remote search coverage or invent a
remote revision. MCP events remain private run-record Artifacts, independently
of the public command/file observation categories below. Unsupported plugins
must expose a capability gap instead of substituting model-authored evidence.

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

## Public operation observations

The qualified Codex adapter advertises `public-command-file-observations.v1`.
It attaches optional typed `progress.observation` metadata to the existing plugin
response and runner execution event. Hosts route these records through the same
bounded ordered queue as output and completion, rather than the replaceable
latest-progress slot. Ordinary progress retains its existing coalescing behavior.
No new runner, plugin or Agent protocol version is required.

Each observation contains an opaque operation digest, an actually reported
`started` or `completed` boundary, the plugin's original observation time, and
allowlisted command or file facts. The enclosing event supplies the original
run/step/command identity, sequence and runner timestamp; the Agent supplies its
existing fenced Job/attempt envelope. A completion does not manufacture a missing
start. Public metadata never contains command text/arguments, environment,
transcripts, prompts, file diffs, output bodies or raw harness identifiers.
Command exit/failure and file failure remain unknown when not reported; these
observations are neither verification evidence nor an acceptance decision.

File observations accept at most 32 changes per harness item. Only portable
workspace-relative paths up to 1,024 UTF-8 bytes are exposed. Absolute,
traversing, hidden, `protected` component and non-portable paths are omitted;
unrecognized change kinds and items without a bounded correlation identifier
produce no observation. MCP calls, web search, reasoning and summaries are not
qualified public operation categories. Consumers must display unsupported or
absent facts as Not reported, rather than parse progress/output text.

Token measurements continue through the existing result/provenance `usage` maps:
`input_tokens`, `cached_input_tokens` and `output_tokens` are separate reported
counters. Missing usage is an empty map, not zero usage or an estimated cost.
This model token cache is distinct from Octa's existing task-result cache
lookup/hit/miss/restore events. Observation metadata introduces no price table,
reservation accounting or additional record store. Existing independently
authorized log readers supply content; generic operation rows contain no output.
