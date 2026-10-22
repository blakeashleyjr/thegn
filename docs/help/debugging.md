---
id: debugging
title: Debugging thegn
order: 32
---

# Debugging thegn

Use this page as the entry point when thegn itself is behaving strangely. It
also describes the deliberately narrow debugger support for programs running
in thegn panes.

## Start with doctor

```sh
thegn doctor
thegn doctor --json
```

The text report is the first pass for terminal capabilities, the resolved
channel and build, daemon reachability, log sinks, crash reports, and other
local diagnostics. `--json` emits the same kind of report as one
machine-readable object, which is useful when collecting a report in a
script.

The **Monitor collectors** block lists each system monitor collector and its
discovered health. Collectors that are checked only while their panel is
visible are marked `visible-only (not probed)`. A `denied` row includes a hint:
RAPL Powercap counters use root-only (0400) `energy_uj` files; other denied
readings need privileges thegn does not request, and thegn never escalates or
prompts.

If you choose to grant a group read access to RAPL counters, install a udev rule
manually. For example, create `/etc/udev/rules.d/99-powercap-energy.rules`
with the following rule, replacing `energy` with a group you control and
`/absolute/path/to` with the directory containing `chgrp` and `chmod` on your
system. Both commands must use absolute paths; for example, NixOS users can use
`${pkgs.coreutils}/bin/chgrp` and `${pkgs.coreutils}/bin/chmod` in
`services.udev.extraRules`.

```udev
ACTION=="add", SUBSYSTEM=="powercap", ATTR{energy_uj}=="?*", RUN+="/absolute/path/to/chgrp energy /sys%p/energy_uj", RUN+="/absolute/path/to/chmod 0440 /sys%p/energy_uj"
```

Then reload the rules and re-trigger the powercap devices, or reboot. This is an
optional administrator action; thegn never installs the rule or changes file
permissions.

The report's `db space` line shows the database size, reclaimable space, and
vacuum mode. It suggests `thegn db vacuum` when the database is not yet in
incremental mode or is too large to convert automatically (more than 64 MiB
of live data).
The line reads the database read-only, including changes still in its
write-ahead log.

For a support archive, run:

```sh
thegn doctor bundle --out /tmp/thegn-bundle.tar.gz
```

The command prints a manifest and writes a local gzip archive containing the
extended doctor report, `config.redacted.toml`, retained crash reports, and
bounded tails of existing `thegn.log`, `thegn-daemon.log`,
`thegn-stderr.log`, and `audit.log` files. Secret scalar values in the
effective config and log text are redacted; each log tail is limited to the
most recent 500 lines. The archive also includes the WARN+ diagnostics ring
from the current bundle process.

That ring is deliberately labeled current-process data: `doctor bundle` is a
separate CLI process, so it cannot take a live ring snapshot from an already
running compositor or pane daemon. The bundle likewise does not turn into a
live daemon diagnostic query; retained crash reports and the available log
tails are the cross-process evidence it can include. Review the manifest and
the extracted bundle before sharing it.

## Logs and raw stderr

`THEGN_LOG` uses tracing-style filters and can select a module or target, for
example:

```sh
THEGN_LOG=thegn::frame=debug,thegn::hydrate=debug thegn
```

For the compositor and pane daemon, requesting tracing logs enables their
rotating file sink under the state log directory (`thegn.log` and
`thegn-daemon.log`). A CLI command can also show its filtered tracing output
on stderr. This tracing stream is separate from raw file-descriptor-2
capture: while the compositor owns the alternate screen, stray writes to
stderr are redirected to `thegn-stderr.log`, with the configured size cap.
The bundle treats those as separate log sinks.

## Performance and profiling

Set `THEGN_PERF=1` before launching the compositor to enable the runtime
`thegn::perf` rollup. It reports wake/render rates, latency, idle and busy
ratios, the hot wake source, PTY throughput, and per-subsystem CPU timing. It
also reports whole-process (`cpu_process_ms`), loop-thread (`cpu_loop_ms`),
scan-pool (`cpu_scan_ms`), reaped-child (`cpu_children_reaped_ms`) and
unattributed (`cpu_unattributed_ms`) CPU. Child CPU is counted when the child
is reaped. Allocator fields report the requested glibc arena cap and whether it
applied, idle and scan-drain trims, released memory, bytes released on macOS,
trim-call time, and drained scan bursts (including throttled ones). The
`malloc_*` counters are cumulative since process start.
`THEGN_MALLOC_ARENA_MAX` sets the glibc arena cap (default 2; 0 = uncapped).
Named worker threads carry `thegn-` names (the Tokio pool is `thegn-rt`), so
`top -H -p <pid>` or `/proc/<pid>/task/*/comm` shows which named thread is busy.
The existing tuning variables are `THEGN_PERF_INTERVAL_MS` and
`THEGN_PERF_WAKE_LIMIT`; `THEGN_PERF_PTY_LIMIT`, `THEGN_FRAME_BUDGET_US`, and
`THEGN_INPUT_BUDGET_US` adjust the related guards. A `THEGN_LOG` filter that
selects `thegn::perf` also enables the accounting.

`input_no_effect_events` counts input keys whose cohort stayed pending while
the loop went idle and was closed by a frame 250 ms or more after the key,
including effects that arrive late from background work. Those keys are
excluded from the input and writer input latency percentiles.

With `THEGN_PERF` enabled, each runtime rollup also writes a thread census to
`thegn::perf::rt`: a summary reports thread and runtime-thread counts plus wake
rates, and up to 12 ranked lines show each thread's id, name, wake and CPU
rates, current syscall, its file-descriptor target, and wait channel. On Linux,
a background thread reads at most 256 entries from `/proc/self/task`; this
bounded read burst runs once per rollup. Other platforms log an empty census.

The live-only view is the **LOOP** overlay in System → Telemetry. Opening that
section enables accounting and rolls it up at its live cadence; it is not a
snapshot that `doctor` or a bundle can reconstruct later.

For a flame graph, use the feature-gated in-process profiler: build with the
`profiling` feature, send `SIGUSR2` to the live process to start sampling, and
send it again to stop. Stopping is asynchronous: a background worker builds and
writes the SVG, so the UI never stalls, and the result (success or failure) is
logged on the `thegn::perf` target. Signals sent while a start or dump is in
flight are coalesced (dropped), never queued. Reports go to the state
`profiles` directory (mode 0700) as `flamegraph-<time>-p<pid>-g<generation>.svg`
(mode 0600), written via an exclusive temp file and never overwriting an
existing report. A failed start leaves the profiler idle, so the next signal
retries. The external profiler path is separate and requires profiling thegn as
a child.

## Debugging users' programs

```sh
thegn debug setup
thegn debug path
thegn debug run <program> [args...]
thegn debug attach <pid>
```

These commands are BugStalker (`bs`) integration only, and are supported only
on Linux x86-64. `setup` installs the pinned tool, `path` reports the resolved
binary, `run` starts a program under it, and `attach` attaches to a running
process. Run the command in a thegn pane when the program needs that pane's
sandbox or placement.

The System → Debug panel entry is a reserved placeholder. This release has no
DAP integration, no gdb/lldb pane integration, and no thegn-managed
breakpoints, stepping, variables, or launch configurations. `thegn doctor` lists
BugStalker under the `debug` provider seam; the plugin `DebugAdapter` (DAP)
extension point is reserved vocabulary and `thegn plugin check` rejects it
until its runtime lands.
