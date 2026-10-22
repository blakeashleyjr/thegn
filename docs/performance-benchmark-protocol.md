# Performance benchmark protocol (THE-643)

## Purpose and status

This document defines a repeatable contract for comparing the performance of thegn. It
contains proposed acceptance thresholds, not results. No value here says that a build
has met a target. Existing sub-300 ms launch-to-first-frame, sub-16 ms render and 0%
idle goals are targets; they are not evidence of current performance. THE-644 will
implement the remaining workload and capture details, THE-667 will integrate the
regression gate, and THE-668 will freeze the comparison baseline and set final values.

The existing metric definitions in [performance-metrics.md](performance-metrics.md)
remain authoritative. In particular, rollup histogram percentiles are lower bucket
edges, zero represents 0–1 µs, and sample counts must be reported. Do not compare
metric-version-2 rollups with older instrumentation as if their boundaries were the
same.

## Measurements and boundaries

“Host” means the interactive thegn process, “daemon” its pane daemon, “child” the
workload process (such as a pane command), and “test group” the harness plus the
processes it explicitly tracks. The owner column names the process whose time or
resource is measured; it does not imply that existing host metrics include its
children.

| Measurement                                        | Start → stop                                                                                                              | Owner                                                                            | Existing reader                                                                                                                                                                                            | Exclusions and interpretation                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| First frame submitted                              | Host launch → submission of its first frame                                                                               | Host                                                                             | `just bench` with `THEGN_BENCH_FIRST_FRAME_EXIT=1`; startup log/metric marker                                                                                                                              | Submission is not successful sink completion or physical paint. The PTY wrapper adds a small constant overhead.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| First useful hydrated model                        | Host launch → first model generation accepted for display with the requested worktree and expected sidebar rows           | Host; fixture records expected state                                             | No live metric for this boundary: THE-644. Hydration fixtures in `crates/thegn-host/src/perf_workloads.rs` and `crates/thegn-host/src/platform/perf_workloads_hydration.rs` cover narrower model workloads | Not the first model build if it is empty, stale, or for a different worktree. Does not include physical paint. Existing fixtures do not measure complete release hydration.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Observed-input cohort → frame submission           | Earliest host observation in a pending cohort → next submitted frame                                                      | Host                                                                             | `input_p*_us`, `input_samples`; `just perf-flood` exercises a flood; `just bench-steady` feeds scripted keystrokes but measures CPU only                                                                   | Input arriving before host observation (device, terminal, reader buffering) is outside coverage. Coalesced events share one cohort/sample; report `input_events` and `input_coalesced_events`. Does not mean the application responded.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Observed-input cohort → successful sink completion | Earliest observation in the submitted cohort → successful completion of its frame write                                   | Host writer                                                                      | `writer_input_p99_us` / `writer_input_samples`; `writer_completion_p99_us` / `writer_frames`                                                                                                               | Same pre-observation exclusion and cohort semantics as above. Failed writes are excluded from success histograms and counted separately. Completion does not acknowledge physical paint or application response.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Tab-switch completion and freshness                | Host observation of switch input → first submitted frame for the destination with its requested worktree/model generation | Host                                                                             | `just perf-t3` reports `switch_ws_p*_us`; `just perf-flood` reports `switch_p*_us` while panes flood                                                                                                       | Switch histograms describe their instrumented switch-to-frame boundary; they do not by themselves prove model freshness. THE-644 must pair latency with the destination-generation correctness check. No physical-paint claim.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| PTY throughput                                     | First byte written by the flooding child → final byte read/accounted by the host during the measured interval             | Child produces; host accounts                                                    | `pty_bytes_per_s` in `just perf-flood` rollups                                                                                                                                                             | This is bytes observed by the host, not terminal paint or useful application work. State the exact flood duration and byte count; do not compare different workloads.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| Whole-process CPU                                  | Same settled measurement-window start and stop for process CPU counters                                                   | Host and daemon separately when present; test group only for aggregate host load | `cpu_process_ms` from `THEGN_PERF=1`; `test/perf/cpu-sample.sh` samples process CPU                                                                                                                        | `cpu_process_ms` covers all host threads but not child-process CPU. Record CPU as cores used over the window and report child CPU separately when available. The soak harness tracks host and daemon separately; do not add an untracked child to either.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Whole-process RSS                                  | Same settled window start and stop for resident-set snapshots                                                             | Host and daemon separately                                                       | No consistent RSS comparison in the current protocol: THE-644                                                                                                                                              | Record peak and end-of-window RSS, process identity, and OS source. Do not substitute virtual size or sum shared pages across processes without saying so.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Per-thread CPU and wakeups                         | Per-thread CPU counter and wakeup counter at the same window boundaries; report deltas and thread identity/lifetime       | Host or daemon thread, grouped under its process                                 | Linux `cpu-sample.sh` provides per-thread CPU; no portable per-thread wakeup reader is currently part of the benchmark contract: THE-644                                                                   | macOS `top` mode reports process CPU only and has no per-thread breakdown. `cpu_*` rollup scope is specific: `cpu_process_ms` covers all host threads; `cpu_loop_ms` is event-loop thread CPU; `cpu_scan_ms` is scan-pool job CPU; `cpu_hydrate_ms` is calling-thread CPU for complete `build_model` invocations; `cpu_hydrate_child_ms` is scoped panel/Git/semantic worker CPU and never child-process CPU; `cpu_children_reaped_ms` is child CPU counted only when reaped; and `cpu_unattributed_ms` is remaining process CPU, floored at zero. `cpu_stats_ms`, `cpu_pr_ms`, `cpu_metrics_ms`, and `cpu_diff_ms` are CPU charged to their corresponding measured subsystem spans. These subsystem fields are scoped work and may overlap thread lifetimes; they are not additional process CPU totals. |
| Producer backlog age                               | Time the oldest queued producer item has waited when the host begins consuming it; report oldest and p95 age per producer | Host queue, with producer named                                                  | No current backlog-age metric: THE-644                                                                                                                                                                     | Do not infer age from queue length or loop idle ratio. Report queue identity and its enqueue/dequeue clock source.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |

All histogram percentiles retain the lower-bucket-edge precision described in
[performance-metrics.md](performance-metrics.md). A value below a threshold is not
more precise than its bucket. `idle_ratio` is event-loop wall-time idle fraction,
not process CPU utilization. Input events without display damage may wait for an
unrelated frame; state whether a fixture guarantees display damage.

## Fixtures

Use the same fixture inputs, workload seed, pane commands, and expected state for both
variants. A fixture listed as “not yet” is part of THE-644, not a claim about current
coverage.

| Fixture                               | Exercises and correctness focus                                                                          | Existing coverage                                                                                                                                                                                       |
| ------------------------------------- | -------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Quiet idle                            | Settled session with no user input; catch event-loop spin, background wakeups, resource growth           | `just bench-idle`; `just bench-soak` adds fixed resource ceilings                                                                                                                                       |
| Processes hidden                      | Processes view not visible; check hidden work is paused and UI remains responsive                        | Not yet (THE-644)                                                                                                                                                                                       |
| Processes paused                      | Explicitly paused process scan; check no hidden scan work and correct resume state                       | Not yet (THE-644); `pause-process-scan.sh` tests harness cleanup of `podman pause` processes, not a paused Processes view                                                                               |
| Processes visible                     | Visible process table under stable process churn; check rows and selection remain current                | Not yet (THE-644)                                                                                                                                                                                       |
| One flooding pane plus one quiet pane | Sustained PTY output competing with switch/input and quiet-pane progress                                 | `just perf-flood` covers multiple flooding panes and switching; a single-flood/quiet-pane pair is not yet covered (THE-644)                                                                             |
| Many panes                            | Concurrent PTY readers/writers and pane fairness                                                         | `just perf-flood` covers multiple flooding panes; a broader pane-count sweep is not yet covered (THE-644)                                                                                               |
| Fresh glyph cache                     | First render after cache is populated; detect warm-path cost                                             | Not yet (THE-644)                                                                                                                                                                                       |
| Expired glyph cache                   | Render after cache expiry/rebuild; check output and cache freshness                                      | Not yet (THE-644)                                                                                                                                                                                       |
| Worktree switching                    | Repeated destination changes across registered worktrees; verify destination model freshness             | `just perf-t3` switches workspaces; `just perf-flood` switches worktrees under output flood                                                                                                             |
| LSP and log bursts                    | Incoming LSP/log activity competing with input and rendering                                             | Not yet (THE-644)                                                                                                                                                                                       |
| Recording                             | Recording enabled with a fixed event stream; verify complete and ordered recording                       | Not yet (THE-644)                                                                                                                                                                                       |
| Slow terminal sink                    | Deliberately rate-limited sink; distinguish submission delay from successful completion delay            | Not yet (THE-644)                                                                                                                                                                                       |
| Hydration scaling                     | Private worktrees at 1, 8 and 32, plus a larger resource-permitting case; report tracked and dirty files | Existing ignored hydration workload uses 1/8/32, 64 tracked text files and one modified file per worktree. Historical unoptimized sequential measurements are limited evidence, not a release baseline. |

For worktree scaling, use 1, 8, and 32 private worktrees and, when resources allow, 64. Record total tracked files and the per-worktree dirty-file distribution (count
and paths or a stable digest of the path list). The historical hydration audit's
64 tracked files and one modified file per worktree are a useful fixture starting
point, not a required production workload.

## Fixed conditions and run manifest

Use a release binary. Each recipe that measures the host runs `_perf-guard`, which
rejects a debug or stale `target/release/thegn`; preserve its resolved binary and
mtime output. Fix terminal geometry, terminal emulator/PTY wrapper, build features
and flags, helper availability, workload seed, fixture state, and run duration for
both variants. Record helper availability as present/absent and keep it the same
within a pair. Do not run competing builds or benchmarks on the host. Run cold-cache
and warm-cache trials as separate cohorts: cold means a fresh process and fresh
private fixture/cache state; warm means the declared state has been exercised once
before the measured launch. Keep glyph-cache freshness/expiry as its own fixture
condition. Do not mix these conditions in one percentile distribution.

Every run artifact includes this manifest (write `unknown` when the harness cannot
read a field; do not omit it):

- variant label and commit ID;
- release profile, enabled features/build flags, resolved binary path and mtime;
- host identity, OS and kernel version, CPU model/count, governor or power mode;
- terminal and PTY wrapper, rows and columns;
- helper availability, fixture name, seed, cache condition, worktree/workspace count;
- tracked-file count and dirty-file count/distribution;
- measured window, settle period, sample count, repeat number, and run order;
- metric version, rollup sample counts, and any unsupported measurement/platform axis.

Use the same host and conditions for a baseline/variant pair. Randomize which variant
runs first within each pair. Keep the raw output and manifest together. If a required
condition differs or is unknown, do not pool the run with the matched cohort.

## Sampling and comparison

Choose counts before looking at results. For each fixture, cache condition, platform,
and input cohort, collect at least 100 observations before quoting p99 and at least
20 before quoting p95. With fewer than 100 independent observations, p99 is the
maximum observation rather than a stable tail estimate; below these minimums report
the percentile as **insufficient samples**, not as a pass. Always include sample
count and the histogram bucket-edge limitation. Use 30 paired repeats as the proposed
minimum for a comparison; report every pair and a paired median delta, not only the
best run. The repeat count is proposed because it provides multiple independent
launches while keeping machine-dependent runs practical; THE-668 may revise it from
collected evidence.

For each repeat, start from the declared cold or warm state, run exactly one fixture
and variant, save its manifest and raw output, then restore the declared starting
state. Randomize baseline/variant order independently per pair. Discard the first
workload observation in each fixture run from percentile calculations, while
retaining and labeling it in raw output. This follows the historical hydration
audit's exclusion of sample zero; it does not make that audit a randomized
comparison.

The protocol harness must preserve events and outcomes, not only latency summaries.
A dropped key, missing frame, stale model, reordered output, or incomplete recording
invalidates the corresponding timing sample. Never count a faster run as a win if
it performed less user-visible work.

## Correctness checks

Apply the common checks to every fixture: all submitted input is accounted for or
explicitly coalesced; expected frame/model generations arrive; output is ordered;
no write failure is silently counted as success; and the final terminal state matches
the fixture's expected state. Also check the fixture-specific conditions:

- Quiet/hidden/paused Processes: no unexpected scans while hidden or paused; visible
  rows and pause/resume state match the fixture.
- Flood and many-pane runs: the quiet pane continues to make progress, output order
  is preserved per pane, and the final pane output matches expected bytes.
- Cache runs: fresh and expired cases actually take their declared cache path and
  produce the same expected glyph/output state.
- Worktree switches: every requested switch is dispatched and the final rendered
  model belongs to the destination worktree generation.
- LSP/log bursts: all fixture messages are consumed in order and the final model
  contains the expected updates.
- Recording: all expected events are present once and in order, and the recording
  closes cleanly.
- Slow sink: all submitted frames either complete successfully or are explicitly
  reported as failed; completion latency is measured only for successful writes.
- PTY throughput: the received byte count and final output agree with the producer's
  expected count and content.

## Thresholds

These are acceptance proposals for a future gate, not measured outcomes. Existing
project goals remain targets. THE-668's frozen baseline and paired evidence set the
final values; THE-667 must not silently turn a proposal into a claim of achievement.

| Metric                                    | Threshold                                                                                                   | Status and rationale                                                                                                                                          |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Launch → first frame submitted            | <300 ms                                                                                                     | Target (`CLAUDE.md`), not a newly measured result.                                                                                                            |
| Render                                    | <16 ms                                                                                                      | Target (`CLAUDE.md`); `THEGN_FRAME_BUDGET_US` defaults to 16,000 µs for the slow-frame warning. Apply only to comparable render samples.                      |
| Idle CPU                                  | 0% idle work as the product goal; current Linux release idle harness ceiling is 0.12 cores                  | Target (`CLAUDE.md`); `bench-idle` uses the fixed 0.12-core ceiling rather than claiming literal zero CPU.                                                    |
| Soak: zombies                             | 0                                                                                                           | Gate (`bench-soak`, `cpu-sample.sh`); any zombie is an unreaped child.                                                                                        |
| Soak: child growth                        | ≤4 over the settled window                                                                                  | Gate (`bench-soak`, `cpu-sample.sh`); coarse allowance for legitimate child fluctuation.                                                                      |
| Soak: descriptor growth                   | ≤8 over the settled window                                                                                  | Gate (`bench-soak`, `cpu-sample.sh`).                                                                                                                         |
| Soak: thread growth                       | ≤2 over the settled window                                                                                  | Gate (`bench-soak`, `cpu-sample.sh`).                                                                                                                         |
| Soak: database descriptors                | ≤16                                                                                                         | Gate (`bench-soak`, `cpu-sample.sh`).                                                                                                                         |
| Soak: peak threads                        | ≤64                                                                                                         | Gate (`bench-soak`, `cpu-sample.sh`).                                                                                                                         |
| Soak: read syscalls                       | ≤5,000/sec                                                                                                  | Gate (`bench-soak`, `cpu-sample.sh`); fixed ceiling applies to procfs scenarios.                                                                              |
| Soak: idle process starts                 | 0 during the idle window                                                                                    | Gate (`bench-soak`, Linux `strace` axis; macOS reports unsupported).                                                                                          |
| Input cohort → frame submission           | p95 ≤16 ms; p99 ≤50 ms                                                                                      | Proposed. The p95 aligns with one frame budget; p99 allows scheduling and batching tail while remaining user-visible.                                         |
| Input cohort → successful sink completion | p95 ≤50 ms; p99 ≤100 ms                                                                                     | Proposed. Allows writer/terminal buffering beyond submission while bounding a slow tail; physical paint remains excluded.                                     |
| Tab-switch completion                     | p95 ≤100 ms; p99 ≤250 ms                                                                                    | Proposed. Keeps ordinary changes responsive and bounds contention tails; freshness must also pass correctness checks.                                         |
| PTY throughput                            | At least 95% of matched baseline bytes/sec                                                                  | Proposed. Relative threshold avoids treating unlike hosts or terminals as comparable absolute rates.                                                          |
| Whole-process CPU and RSS                 | No more than 5% regression against matched baseline median                                                  | Proposed. A small relative budget catches material resource growth without claiming a universal hardware-independent absolute. Report CPU and RSS separately. |
| Per-thread CPU and wakeups                | No more than 10% regression for the responsible thread group against matched baseline                       | Proposed. Diagnostic attribution threshold; only compare the same OS reader and thread role.                                                                  |
| Producer backlog age                      | p95 ≤50 ms; p99 ≤100 ms                                                                                     | Proposed. Bounds queued work behind one to several frame opportunities; correctness still requires eventual drain.                                            |
| Worktree scaling                          | No more than 10% latency regression from 1 to 8 or 8 to 32 beyond the frozen baseline's corresponding slope | Proposed. Compares scaling shape rather than assuming linear costs are avoidable.                                                                             |

The existing soak ceilings are fixed harness gates, not baseline-derived performance
thresholds. The 0.12-core ceiling applies to the release idle scenario; resource
ceilings are enforced on procfs scenarios. The resource ceilings and their scope are
defined by `test/perf/cpu-sample.sh`; they are not claims about platforms or axes the
script reports as unsupported.

## Procedure

The following existing recipes are runnable after building a release binary. They
are advisory except where the recipe itself applies its fixed soak/idle gate:

```sh
just bench
just bench-idle
just bench-steady
just perf-flood
just perf-t3
just bench-soak
just bench-soak-daemon
just bench-micro
```

For the release first-frame comparison, `just bench` runs hyperfine with warmups and
isolated `XDG_STATE_HOME`, including cold-state and warm-state launch recipes. Preserve
the command output and environment. `just bench-idle` and `just bench-steady` use the
existing CPU harness. `just perf-flood` and `just perf-t3` read current rollup fields;
`just bench-soak` and `just bench-soak-daemon` enforce their fixed resource ceilings.
`just bench-micro` covers Criterion microbenchmarks, not interactive latency.

Until THE-644 automates paired manifests and missing fixtures, perform these manual
steps for each baseline/variant pair:

1. Build the release binary for each commit and retain `_perf-guard` output. Use the
   same build flags/features and helper setup.
2. Prepare the fixture and declared cache state. Record geometry, worktree count,
   tracked/dirty distribution, seed, and helper presence in the manifest.
3. Randomize variant order, run each selected recipe once per pair, retain raw logs
   and JSON, then restore the fixture before the next pair.
4. Repeat at least 30 pairs; keep the first launch warm-up sample but exclude it from
   percentile calculations. Collect 100 observations per cohort for p99 and 20 for
   p95; otherwise mark the percentile insufficient.
5. Verify every correctness condition before including timing data. Compare only
   manifests with matching host, OS, geometry, workload, cache condition, and build
   settings. Report paired deltas, sample counts, and unsupported axes.

THE-644 should automate fixture setup, randomized pairing, manifest capture,
correctness validation, and the listed measurements that have no reader today. This
protocol requires no new always-on benchmark and no new telemetry timer. Existing
profiling and rollup instrumentation remains opt-in; any missing measurement can be
collected by the harness or a controlled external sampler.
