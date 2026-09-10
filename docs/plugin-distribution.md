# Reproducible plugins

Octa can run plugins directly for local development or verify an exact plugin set before any
plugin process starts. Agent executions should always use the verified mode.

Each distributed plugin binary has a sidecar `*.plugin.yml` manifest:

```yaml
manifest_version: 1
name: shell
version: "0.3.0"
protocol: 1
platforms: [linux-x86_64]
entrypoint: octa_plugin_shell
sha256: 4f5c...
capabilities: [shell]
```

`entrypoint` is a file name relative to the plugin directory. Parent components, absolute paths,
and symlinks leaving that directory are rejected. `platforms` uses Rust OS and architecture names,
for example `linux-x86_64`, `windows-x86_64`, and `macos-aarch64`.

Create and verify a deterministic lock:

```console
octa plugin lock --output Octa.lock
octa plugin verify --lock Octa.lock
```

The plugin directory defaults to `plugins` below the workspace and can be changed with
`OCTA_PLUGINS_DIR`. Lock creation reads every `*.plugin.yml`, validates protocol/platform/digest,
sorts entries by logical plugin name, and writes version 1 YAML. Verification hashes every complete
binary before it can be launched.

Local execution opts into the lock explicitly:

```console
octa --plugin-lock Octa.lock build
```

The runner accepts `request.plugin_lock`; a relative value is resolved below `request.workspace`.
When supplied, the lock must contain the built-in `shell` and `tpl` plugins as well as every plugin
named in `request.plugins`. No unlocked binary is used as a fallback.

Release archives contain platform-specific manifests, `Octa.lock`, and an archive SHA-256 file.
Fetching and unpacking a release is intentionally outside Octa: a future agent or package manager
owns distribution, while Octa owns verification and execution.
