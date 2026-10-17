# Design

## Sampling and ownership

The existing base `StatsSampler` remains small and always-on for masthead
statistics. A separate deep `MonitorSampler` starts when the monitor opens,
stops and joins when it closes, and parks without timed waits while the monitor
is hidden or paused. The existing process worker, container inventory, and
UDisks2 integration are exceptions with their own established lifecycles. The
base `StatsSnapshot` is not widened to carry deep-monitor data. Published
frames use a three-frame pool and latest-wins delivery; a worker pulses the
`TerminalWaker` at most once for each published frame.

The deep sampler owns registered collectors. Three existing sources remain
outside it and enter monitor frames through
`post_external(CollectorId, Part)`: the parked process worker, container
inventory from its hydrate refresh, and UDisks2 SMART data from the host async
runtime. This seam lets the board compose those readings without moving
blocking work onto the event loop.

## Module layout

The metrics collector tree is
`thegn-metrics/src/collect/{parse,model,policy,linux,macos,windows}`. Pure
parsers, models, and policy live in their named subtrees; platform readers live
under the platform subtrees. The only platform `cfg` declarations are the
`mod` lines in `collect/mod.rs`, which is the single platform-cfg ratchet pin.
Process `procs_parse.rs` and `procs_model.rs` are flat siblings of `procs.rs`.

The host monitor owns `monitor/{layout,board,kit,tabs}`: the solver, board
integration, shared widgets, and mechanically separated tab modules. The core
owns `numfmt.rs`, `viz_raster.rs`, and `series_store.rs` for numeric formatting,
graph rasterization, and bounded history.

## Shared-reader ownership

Locations are relative to `thegn-metrics/src/collect/` unless stated otherwise.

| Reader                                      | Owning lane      | Location                                                           |
| ------------------------------------------- | ---------------- | ------------------------------------------------------------------ |
| PSI parser and system reader                | F3b              | `parse/psi.rs`, `linux/psi.rs`                                     |
| `/proc/stat`                                | CPU, SR-1        | `parse/stat.rs`, `linux/cpu_stat.rs`                               |
| cpufreq and CPU policy                      | CPU, SR-2        | `linux/cpufreq.rs` (absorbs P0-03 interim reader)                  |
| hwmon                                       | Thermal, SR-3    | `linux/hwmon.rs`, `policy/hwmon_group.rs`                          |
| cgroup reader and classification            | Containers, SR-4 | `parse/cgroup.rs`, `linux/cgroup*.rs`, `policy/cgroup_classify.rs` |
| amdgpu `gpu_metrics` decoder and SMU reader | GPU, SR-5        | `parse/amd_gpu_metrics.rs`, `linux/amd_smu.rs`                     |
| Process parsing and model                   | Processes, SR-6  | `procs_parse.rs`, `procs_model.rs`, alongside `procs.rs`           |
| meminfo and vmstat                          | Memory, M-1      | `parse/{meminfo,vmstat}.rs`, `linux/{meminfo,vmstat}.rs`           |
| `/proc/net/dev`                             | Network, N-1     | `parse/netdev.rs`, `linux/netdev.rs`                               |
| diskstats and mountinfo                     | Disk, D-1        | `parse/{diskstats,mountinfo}.rs`, `linux/diskstats.rs`             |
| powercap                                    | Power, T-3       | `linux/powercap.rs`                                                |
| numeric formatting                          | Framework, F1a   | `thegn-core/src/numfmt.rs`                                         |

Memory's interim `proc_mem.rs` reader is dropped; process memory detail is
owned by the process lane. The interim P0-03 cpufreq reader is absorbed by
SR-2. These owners provide each shared read once for all consuming panels.

## Keys and presentation

Overview is tab 1 and the default only when no saved tab exists. Persist
`last_tab` by slug, and let a valid saved slug win so existing users retain
their destination. Enter opens detail everywhere and returns to a row's home
tab when necessary; Containers shell moves to `e`. `f`/`F` changes panel focus,
`z` zooms, and `w` changes grouping. Escape clears a filter or confirmation,
then leaves detail, then zoom, then closes the monitor. Containers `t` and `r`
require a y/n confirmation. These keys introduce no new `ActionSpec` ids.

Colours and glyphs use semantic roles and capability-selected glyphs at the
existing rendering chokepoints. Values use the shared numeric formatter rather
than panel-specific number formatting.

## History and layout

History uses three bounded tiers: T0 stores 600 one-second samples; T1 stores
720 five-second buckets with mean and maximum; T2 stores 1440 sixty-second
buckets with mean and maximum. Class A, B, and C series have explicit caps and
share a compile-time 8 MiB budget. Pushes allocate nothing. Missing buckets are
NaN gaps, never zero, and history is not persisted across restarts.

The layout solver tiles the interior with one shared border and no overlap.
It downgrades panel presentations before dropping panels by priority, with a
deterministic, monotonic result and a `+N hidden` count. Below 56×16 it uses a
stacked scrolling fallback. Absent panels leave the solver before layout;
warming panels show `…`, while denied panels show one ghosted reason line. The
board targets a fill ratio of at least 0.55 at 240×60.

## Frame pipeline and budgets

Sampling publishes data frames independently of painting. The monitor frame is
chrome damage and uses render channel `Full`; an idle wake with no frame remains
`Skip`. The sampler publishes latest-wins through the three-frame pool, then
pulses the terminal waker at most once for that frame. The event loop gains no
timer or polling timeout. Graph rasters are calculated only on repaint. In a
release build at 240×60, board rebuild is budgeted at 2 ms and blit at 0.3 ms,
with zero allocations after warm-up. The perf workload measures these budgets;
wall-clock CI does not.

There is no SQLite schema change.

## Help and spec reconciliation

The interactive surface uses help context `overlay:monitor`, mapped to
`docs/help/system-monitor.md`. Framework target behaviour belongs in this
change's delta spec; that page documents only behaviour shipped on the base.
SPEC-1 must MODIFY the main-spec requirement "All system sampling runs off the
event loop" to admit the `MonitorSampler`. The existing "Hidden process
sampling is parked and owned" requirement remains unchanged.
