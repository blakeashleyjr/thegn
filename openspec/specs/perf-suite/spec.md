# Performance Suite

## Purpose

thegn ships a performance toolkit — a runtime self-profiler, a steady-state
idle harness, micro-benchmarks, a live telemetry overlay, and an in-process
flame-graph profiler — that is entirely free when disabled. The machine-dependent
benchmarks are excluded from CI; the render-decision invariants are instead locked
as pure unit tests that CI does run.

## Requirements

### Requirement: The runtime self-profiler is free when off

Profiling SHALL be gated (e.g. `THEGN_PERF` / a build feature) so it imposes zero cost when disabled, and when enabled it MUST emit a `thegn::perf` rollup with wake-source and per-subsystem CPU attribution plus a wake-storm warning.

#### Scenario: Disabled imposes no cost

- **WHEN** the profiler is disabled
- **THEN** no profiling subscriber is installed and no per-frame profiling work runs

#### Scenario: Enabled emits a rollup

- **WHEN** the profiler is enabled
- **THEN** a `thegn::perf` rollup with wake-source and per-subsystem CPU
  attribution is produced, warning on a wake storm

### Requirement: Slow-frame warning on cost-per-frame regressions

The runtime SHALL emit a slow-frame warning when `render_p50_us` exceeds `THEGN_FRAME_BUDGET_US` (default 16ms) and report a render busy ratio.

#### Scenario: Frames exceed the budget

- **WHEN** median render time exceeds the configured frame budget
- **THEN** a slow-frame warning is emitted

### Requirement: Soak output reports host and daemon resident memory

The soak harness SHALL publish peak resident memory for the host and, when the
scenario starts one, the pane daemon, in KiB, naming the OS source. A fixed RSS
ceiling SHALL be enforced only after a measured run fixes its value; the harness
MUST NOT invent a default ceiling.

#### Scenario: RSS is reported

- **WHEN** a soak run samples a procfs-backed host and pane daemon
- **THEN** its JSON includes `rss_host_kb` and `rss_daemon_kb` in KiB

#### Scenario: No measurement fixes the ceiling

- **WHEN** no measured RSS baseline has been recorded
- **THEN** the RSS gate is unset and the run reports the value without failing on it

### Requirement: Benchmarks are opt-in and excluded from CI

The idle harness, micro-benchmarks, and flame-graph profiler SHALL be opt-in tools excluded from `just ci` because they are machine-dependent, while the render-decision invariants MUST be enforced in CI as pure unit tests.

#### Scenario: CI runs invariants but not benches

- **WHEN** `just ci` runs
- **THEN** the render-plan invariant unit tests execute while the wall-clock
  benchmarks do not

### Requirement: Performance measurements disclose their boundaries

Rollups SHALL identify their metric version, elapsed interval and sample counts.
Submission timings MUST remain distinct from successful sink-write completion;
neither SHALL claim physical display or application-response acknowledgement.

#### Scenario: Output is queued behind a slow sink

- **WHEN** a composed frame is queued before its bytes can be written
- **THEN** submission and successful completion are measured independently
- **AND** failed writes and out-of-band output do not count as successful frames

### Requirement: Published release claim runs retain raw measurements

The release claim harness SHALL measure an existing binary without building it,
retain raw launch and harness JSON for every run, and write a manifest and summary.
Its manifest SHALL identify the version/tag, platform class, OS, CPU, terminal,
geometry, worktree counts, cache state, and sample counts. Flood latency
percentiles SHALL disclose their rollup sample counts and be marked insufficient
when fewer than 100 samples were collected.

#### Scenario: Measuring a published release

- **WHEN** the release claim harness runs against a published binary
- **THEN** it reports cold and warm first-frame launches, idle cores, flood
  switch/input latency, and host/daemon RSS for 1, 8, and 32 worktrees
- **AND** it writes raw JSON and a paste-ready Markdown summary
- **AND** it does not claim histogram lower bucket edges are exact timings

### Requirement: Input and CPU accounting preserve observed work

The earliest pending input observation SHALL survive dispatch and coalescing.
Busy wall time SHALL include synchronous dispatch and exclude blocking poll wait.
Hydration parent and scoped-child thread CPU SHALL be distinguished without
double-counting parent-thread spans or implying subprocess CPU coverage.

#### Scenario: Dispatch continues directly to the next iteration

- **WHEN** a synchronous handler performs work and continues the loop early
- **THEN** its active time is counted, and its earliest pending input remains

#### Scenario: Writer instrumentation is idle

- **WHEN** no existing loop event requires a rollup
- **THEN** writer measurements add no timer or success wake
