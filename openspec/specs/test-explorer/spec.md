# Test Explorer

## Purpose

thegn discovers and runs a worktree's tests through a general task/result
substrate and surfaces them as an IDE-style explorer: a lazily-discovered target
tree, run-selected/all/failed actions, jumpable failure locations, and per-worktree
rollups. Discovery is cheap and cached, runs never happen automatically, and every
run is resource-capped.

## Requirements

### Requirement: Discovery is lazy and cached

Worktree switches SHALL perform only cheap manifest sniffing; expensive test-target discovery runs only when the Tests panel opens or the user refreshes, and results are cached per worktree.

#### Scenario: Switch stays cheap

- **WHEN** the user switches to a worktree
- **THEN** only cheap manifest sniffing runs; full target discovery does not run
  until the panel opens or the user refreshes

### Requirement: Multi-ecosystem ingestion with jumpable failures

Test results SHALL be ingested via text, JSON, and report (JUnit XML / TRX) parsers across the supported ecosystems (cargo/nextest, go, pytest, jest/vitest, and others) with a nix-flake-check fallback, and failures MUST expose a jumpable `file:line` location.

#### Scenario: Failure is jumpable

- **WHEN** a test fails and its output yields a file and line
- **THEN** the failure can be opened at that `file:line` in the editor

### Requirement: Tests never auto-run and are resource-capped

Test runs SHALL be explicit (run selected / all / failed); a file change MUST only mark results stale, never trigger a run; and each discovery/run child MUST be wrapped in a CPU/memory cap with single-flight per-worktree cancellation of a superseded run.

#### Scenario: File change marks stale, does not run

- **WHEN** a file in the worktree changes
- **THEN** cached results are marked stale and no test run is started

#### Scenario: Superseding run cancels the prior

- **WHEN** a new run starts for a worktree while one is in flight
- **THEN** the prior run's process group is killed and the new run proceeds

### Requirement: Latest results persist for rollups

The latest per-target results and rollups SHALL persist per worktree so sidebar/statusbar status survives tab switches and restarts; full historical runs are not persisted.

#### Scenario: Rollup survives restart

- **WHEN** the host restarts after a test run
- **THEN** the worktree's latest pass/fail rollup is restored without re-running
