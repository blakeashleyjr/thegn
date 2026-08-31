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
ratios, the hot wake source, PTY throughput, and per-subsystem CPU timing.
The existing tuning variables are `THEGN_PERF_INTERVAL_MS` and
`THEGN_PERF_WAKE_LIMIT`; `THEGN_PERF_PTY_LIMIT`, `THEGN_FRAME_BUDGET_US`, and
`THEGN_INPUT_BUDGET_US` adjust the related guards. A `THEGN_LOG` filter that
selects `thegn::perf` also enables the accounting.

The live-only view is the **LOOP** overlay in System → Telemetry. Opening that
section enables accounting and rolls it up at its live cadence; it is not a
snapshot that `doctor` or a bundle can reconstruct later.

For a flame graph, use the feature-gated in-process profiler: build with the
`profiling` feature, send `SIGUSR2` to the live process to start sampling, and
send it again to write an SVG under the state `profiles` directory. The
external profiler path is separate and requires profiling thegn as a child.

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
breakpoints, stepping, variables, or launch configurations.
