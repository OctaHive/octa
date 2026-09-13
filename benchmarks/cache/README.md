# Task-result cache performance suite

This directory contains the reproducible Phase 10 gate for Octa's task-result
cache. `run.py` creates deterministic fixtures and retains every raw process
sample; `report.py` is the separate policy boundary that applies the fixed
values in `thresholds.json` and `thresholds.md`.

The release matrix covers 1, 1,000, and 100,000 inputs, 1 GiB streams, 10,000
outputs, local miss/hit behavior, loopback and delayed HTTPS, concurrent runner
consumers, parallel and overlapping task trees, plugin-provided contracts, a
compiler task, two absolute workspaces, go-task, and an exact pre-cache Octa
binary. Remote publication and hits record byte, request, and route counters.
Process samples retain wall/CPU time, p95 inputs, peak RSS, block-I/O operation
counts, cache layer, restored bytes, and miss reason. The Rust kernel probe
separately measures hashing, packing, extraction, and compression without
CLI/plugin startup noise.

Local fixtures report two warm paths: `current-materialized` verifies unchanged
inputs while leaving outputs in place and is compared directly with go-task;
`current-warm` deletes the output before every sample and measures Octa's full
transactional CAS restoration, for which go-task has no equivalent operation.

Build current and baseline release tools, then run:

```bash
cargo build --release \
  -p octa-cli -p octa-runner \
  -p octa_plugin_shell -p octa_plugin_tpl
cargo build --release -p octa-cache --example cache_kernel_bench

python3 benchmarks/cache/run.py \
  --octa target/release/octa \
  --runner target/release/octa-runner \
  --kernel target/release/examples/cache_kernel_bench \
  --plugins target/release \
  --baseline-octa /path/to/pre-cache/target/release/octa \
  --baseline-plugins /path/to/pre-cache/target/release \
  --output benchmarks/cache/results/YYYY-MM-DD-platform

python3 benchmarks/cache/report.py \
  benchmarks/cache/results/YYYY-MM-DD-platform --strict
```

`--quick` preserves all workload shapes at development sizes and is useful for
orchestration checks, but its release-size checks are reported as skipped.
`--resume` accepts only an output directory with an intact run identity whose
host, sizes, concurrency settings, executables, plugins, thresholds, and TLS
fixtures match the current invocation. The identity and every scenario are
written atomically, so interruption cannot turn a partial document into valid
evidence.

The Python measurement helper uses POSIX process-resource accounting. The
performance suite therefore runs on Linux and macOS; Windows behavior remains
covered by the Rust workspace tests and platform CI.

The manual `Cache performance` workflow runs the same strict command only on a
dedicated runner carrying the `octa-performance` label. GitHub-hosted machines
are intentionally unsuitable as the stable Linux evidence class.

Distributed compiler tool-cache evidence belongs to the independently deferred
Phase 7 and is not part of this task-result-cache gate. Phase 10 requires both
developer-machine and stable-Linux raw result sets to pass.
