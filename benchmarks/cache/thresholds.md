# Task-result cache performance thresholds

These thresholds are fixed before reviewing cache implementation results.
Measurements use release binaries, three warmups, at least 15 samples, an idle
machine, and the median unless a p95 bound is named explicitly.

## Existing behavior

- A scenario without a `cache` block may regress by at most 5% against the
  preimplementation Octa binary.
- The geometric mean of all comparable non-cache current/baseline ratios may
  not exceed 1.03.
- A non-cache task must not scan or open cache inputs, outputs, action records,
  or blobs.
- Any scenario slower than go-task must remain visible in the generated report;
  it is not discarded as an incomparable result.

## Cache work

- Streaming hashing of one large file must sustain at least 300 MiB/s on the
  stable Linux agent and the recorded developer machine, excluding the initial
  cold filesystem read.
- Deterministic bundle packing and extraction must each sustain at least 100
  MiB/s for large data on those machines.
- Peak RSS while hashing, packing, or extracting a 1 GiB object must remain
  below 256 MiB above the no-cache process baseline.
- Processing 100,000 inputs or 10,000 outputs must have bounded memory; peak RSS
  must not grow by more than 1 KiB per entry plus the 256 MiB fixed allowance.
- A one-file warm local result hit must take no more than 1.50 times the slower
  of the preimplementation Octa freshness hit and the equivalent go-task warm
  run. This comparison is intentionally reported even though only Octa restores
  deleted output contents.
- A warm local hit that restores 10,000 outputs must be faster than rerunning
  the fixture command, and its p95 must be no more than 1.25 times its median.
- Twenty-five parallel cacheable tasks must share one configured hashing
  budget; observed concurrent file hashing may not exceed that budget.
- Overlapping input sets in one process must not read the same file content
  more than once while the metadata identity remains unchanged.
- A remote hit over loopback may issue a constant number of metadata requests
  plus one request per bundle, never one request per output file.

Failures require either an implementation fix or an explicit revision to this
document with benchmark evidence and rationale. Thresholds must not be loosened
silently after results are known.
