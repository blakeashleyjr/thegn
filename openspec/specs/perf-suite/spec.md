# Performance Suite

## Purpose

superzej ships a performance toolkit — a runtime self-profiler, a steady-state
idle harness, micro-benchmarks, a live telemetry overlay, and an in-process
flame-graph profiler — that is entirely free when disabled. The machine-dependent
benchmarks are excluded from CI; the render-decision invariants are instead locked
as pure unit tests that CI does run.

## Requirements

### Requirement: The runtime self-profiler is free when off

Profiling SHALL be gated (e.g. `SUPERZEJ_PERF` / a build feature) so it imposes zero cost when disabled, and when enabled it MUST emit a `szhost::perf` rollup with wake-source and per-subsystem CPU attribution plus a wake-storm warning.

#### Scenario: Disabled imposes no cost

- **WHEN** the profiler is disabled
- **THEN** no profiling subscriber is installed and no per-frame profiling work runs

#### Scenario: Enabled emits a rollup

- **WHEN** the profiler is enabled
- **THEN** a `szhost::perf` rollup with wake-source and per-subsystem CPU
  attribution is produced, warning on a wake storm

### Requirement: Slow-frame warning on cost-per-frame regressions

The runtime SHALL emit a slow-frame warning when `render_p50_us` exceeds `SUPERZEJ_FRAME_BUDGET_US` (default 16ms) and report a render busy ratio.

#### Scenario: Frames exceed the budget

- **WHEN** median render time exceeds the configured frame budget
- **THEN** a slow-frame warning is emitted

### Requirement: Benchmarks are opt-in and excluded from CI

The idle harness, micro-benchmarks, and flame-graph profiler SHALL be opt-in tools excluded from `just ci` because they are machine-dependent, while the render-decision invariants MUST be enforced in CI as pure unit tests.

#### Scenario: CI runs invariants but not benches

- **WHEN** `just ci` runs
- **THEN** the render-plan invariant unit tests execute while the wall-clock
  benchmarks do not
