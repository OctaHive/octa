# Agent Ready performance suite

This suite compares release binaries using equivalent local-only DAGs. It
covers startup/no-op, a shell command, a DAG-only 25-node chain, no-op and
effectful 25-node shell chains, wide DAGs, sequential and parallel scheduling,
large stdout/stderr, and the additional `octa-runner` process/protocol overhead.
Legacy cold/warm freshness scenarios are no longer executable after the task
result cache replaced `sources` and `output`; their recorded results remain a
historical comparison for the cache suite in `../cache/`.

Build release binaries first, then run:

```sh
python3 benchmarks/agent-ready/run.py \
  --octa target/release/octa \
  --runner target/release/octa-runner \
  --plugins target/release \
  --baseline-octa /path/to/baseline/octa \
  --baseline-plugins /path/to/baseline/plugins \
  --output benchmarks/agent-ready/results/local
```

The go-task binary is resolved from `PATH`; pass `--task` to pin another one.
The gate uses 3 warmups and 15 measured executions by default. Every scenario
is exported in hyperfine JSON with raw timings. `metadata.json` records tool
versions, commit, dirty state, OS, architecture, and run parameters.

Generate a compact JSON and Markdown report from a completed run:

```sh
python3 benchmarks/agent-ready/summarize.py benchmarks/agent-ready/results/local
```

Use a clean, otherwise idle machine. Network is not used by fixtures, and
go-task runs with `--offline`. A release comparison should run this unchanged
on the target Linux agent class as well as a developer machine. Results are a
performance signal, not a requirement that Octa win every go-task case.
