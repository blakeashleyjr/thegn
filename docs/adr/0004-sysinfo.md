# ADR-0004: Keep sysinfo as the selective metrics provider

- Status: Adopted / keep
- Date: 2026-08-29
- Scope: Periodic system metrics, targeted process metrics, and hostname

## Context

`thegn-metrics` directly owns feature-trimmed `sysinfo 0.39`
(`crates/thegn-metrics/Cargo.toml:16-25`), while `thegn-core` uses it only on
non-Linux targets (`crates/thegn-core/Cargo.toml:88-93`). `StatsSampler` reuses
one configured `System`, refreshes cheap fields each tick, and slow fields
every fifth tick (`crates/thegn-metrics/src/sample.rs:1-23,46-88`). Sampling
runs off the event loop (`crates/thegn-host/src/hydrate.rs:608-613`; the
Observe path has its own sampler thread at `crates/gtui-query/src/host.rs:31-58`).

Targeted process refresh deduplicates PIDs after a prior duplicate-PID
double-close abort (`crates/thegn-metrics/src/sample.rs:339-417`), while the
full process sampler has a separate two-second gate
(`crates/thegn-metrics/src/procs.rs:1-21,157-172`). The macOS activity path
uses direct libproc instead of a whole-table sysinfo refresh
(`crates/thegn-core/src/activity.rs:664-683`); the measured rationale and
regression are recorded in `CHANGELOG.md:880-900`.

## Decision

Adopt / keep. Sysinfo is the cross-platform cold/periodic sampler, not a
permission to enumerate processes every tick. Keep GPU, battery, and
Apple-specific thermal providers at their existing edges
(`crates/thegn-metrics/src/lib.rs:1-24`). Feature trimming controls binary and
build cost; MSRV 1.89 is already the workspace floor, and the dependency is
already paid in the Linux/musl and cross-target leaves. Replacing it would be
a substantive migration with no measured gain.

## Reopen condition

Any replacement must be measured and occur behind `StatsSampler`/`ProcSampler`,
preserving background ownership, PID deduplication, selective refresh, and
missing-metric degradation. It must be checked for the musl, macOS, and mingw
lanes before changing the direct dependency.
