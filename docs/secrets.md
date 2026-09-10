# Secret providers

Octafile stores logical references, not secret values. A separate profile maps
stable provider aliases to the credentials and endpoints available in the
current environment. The same Octafile can therefore run locally and through
`octa-runner` without changing task configuration.

```yaml
# Octafile.yml
version: 1

vars:
  REGISTRY_TOKEN:
    secret:
      provider: application
      key: registry/credentials
      field: token

tasks:
  login:
    shell: registry-cli login --token "{{ REGISTRY_TOKEN }}"
```

Pass the environment-specific profile to the CLI or runner:

```sh
octa --secrets-profile .octa/secrets.local.yml login
```

`OCTA_SECRETS_PROFILE` is the CLI equivalent. A runner `Start` request uses
`secrets_profile`; it contains a path relative to the workspace or an absolute
path, never resolved values.

## Profile format

Profiles use version 1. Provider names are aliases referenced by Octafile.
Resolved values are cached only in memory for the runtime lifetime; the
default TTL is five minutes.

```yaml
version: 1
cache_ttl_seconds: 300
providers:
  application:
    type: vault
    address: https://vault.example.test
    mount: secret
    kv_version: 2
    namespace: engineering
    auth:
      type: jwt
      role: octa-ci
      jwt_path: /var/run/secrets/vault.jwt
```

Supported providers:

- `env`: reads `prefix + key` from the Octa/runner process environment.
- `file`: reads a UTF-8 file below a configured root. Absolute keys, traversal,
  and symlink escapes are rejected.
- `exec`: starts a configured helper, sends the serialized `SecretRef` JSON on
  stdin, and treats bounded stdout as the value.
- `vault`: reads Vault KV v1 or v2. Static token authentication reads the token
  from a named environment variable. JWT authentication logs in using a file,
  renews renewable session-owned tokens, and revokes them during shutdown.

Examples:

```yaml
version: 1
providers:
  application:
    type: env
    prefix: OCTA_SECRET_
  files:
    type: file
    root: ./private
  helper:
    type: exec
    command: [./bin/resolve-secret, --format=json]
    timeout_seconds: 15
  vault_static:
    type: vault
    address: https://vault.example.test
    mount: secret
    kv_version: 2
    auth:
      type: token
      token_env: VAULT_TOKEN
```

For `env`, `file`, and `exec`, `field` parses the returned UTF-8 value as JSON
and selects one top-level field. Vault selects the field directly from the KV
document. Without `field`, scalar providers return a string and Vault returns
the complete data object.

## Security boundary

The runner protocol transports only the profile path and logical references.
Resolved values are marked secret before template/plugin execution, redacted
from normal stdout, stderr, progress, diagnostics, errors, terminal results,
and persistent freshness metadata. Raw/PTY execution is rejected when resolved
secrets are present because an arbitrary terminal byte stream cannot be
reliably scrubbed.

Plugins execute inside the job trust boundary and receive values needed by the
task. A plugin can intentionally exfiltrate data through files or the network;
capability declarations are descriptive, not a security boundary. Job-level
sandbox and network policy remain the agent's responsibility. Artifact files
are not scanned for secrets, so the agent must treat their contents as
untrusted before upload.

Provider responses and files are limited to 1 MiB. Octa does not print provider
response bodies, JWTs, or tokens in errors. A profile itself may contain paths,
endpoints, and helper commands and should be protected as configuration even
when credentials come from workload identity.
