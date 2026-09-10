# Agent Ready performance thresholds

These thresholds were fixed before reviewing the completed 15-run result set.
They compare median wall-clock time from identical release-binary fixtures.

- No individual current-Octa scenario may regress by more than 20% against the
  baseline commit without an investigation and written explanation.
- The geometric mean of current/baseline median ratios across comparable Octa
  scenarios must not exceed 1.10.
- Median `octa-runner` overhead against the CLI in the no-op fixture must not
  exceed 20%. A negative overhead is treated as measurement noise, not a gain.
- p95 must not exceed the baseline by more than 30% in two or more scenarios.
- go-task is a reference, not a pass/fail threshold. Material losses must still
  be called out in the report so optimization priorities remain visible.

The local run is useful for regression detection. The same unchanged suite
must run on the stable Linux agent class before tagging an Agent Ready release.
