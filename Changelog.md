## 0.3.0 - 2026-09-04

### Highlights

- Added automatic monorepo discovery with cached project hierarchies and colon-based task namespaces.
- Added task and command conditions, deferred commands, timeouts, fail-fast execution, watch mode, and configurable concurrency.
- Added output-aware freshness checks, source and output exclusions, hash and timestamp strategies, and hierarchical `.octaignore` files.
- Added ordered, required, interactive, secret, shell-backed, and dynamically constrained variables, including CLI variable overrides.
- Added Octafile- and task-level dotenv loading, plugin-backed template evaluation, and file-backed template tasks.
- Added platform and architecture selectors for tasks, commands, and interpolated include paths.
- Added plugin task annotations, plugin-provided validation schemas, and configurable default command plugins.
- Added task search, default task execution, improved Octafile discovery, and automatic creation of task directories.
- Replaced platform-specific command shells with the cross-platform Brush shell and bundled commonly used core utilities.

### Important changes

- Shell commands now use Bash-compatible Brush syntax on every platform. Windows `cmd.exe` syntax such as `%NAME%`, `if exist`, and `exit /B` must be replaced with their Bash equivalents.
- The shell plugin bundles `base64`, `cat`, `cp`, `ls`, `mkdir`, `mktemp`, `mv`, `rm`, `sleep`, and `touch`, so these commands no longer require separate installation on Windows.

## 0.2.0 - 2025-01-20
Support for a plugin system for executors within tasks has been implemented. Built-in functionality for running the templating engine and shell commands has been moved to plugins

- First release

## 0.1.0 - 2024-12-22

- First release
