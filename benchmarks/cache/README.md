# Task-result cache performance suite

This directory defines the performance gate for Octa's task-result cache. The
thresholds were written before cache execution results were available. Raw
hyperfine samples and machine metadata belong under `results/`, matching the
existing Agent Ready suite.

Phase 5 now makes local cache scenarios runnable through the CLI. The complete
cross-platform harness and recorded measurements intentionally remain the Phase
8 performance gate; `../agent-ready/run.py` continues to provide the frozen
startup/non-cache/go-task baseline until that harness lands. Historical
freshness samples are retained only as comparison data.

The final suite must cover one, 1,000, and 100,000 inputs; a large input;
10,000 outputs; cold and warm local execution; loopback remote execution; 25
parallel cacheable tasks; overlapping input sets; and equal actions in two
absolute workspaces.
