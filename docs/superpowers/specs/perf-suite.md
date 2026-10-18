# Performance suite — benchmarking, tracing & profiling

The tools for ruthlessly tracking down thegn performance issues. Built after an
idle-CPU regression (~1.1–1.5 cores at "idle", debug build) could only be found
by hand with `/proc` sampling and a throwaway bench. Five pieces, each free when
not in use.

## 1. Runtime self-profiler (`thegn::perf`)

`crates/thegn-host/src/perf.rs`. A process-global atomic switch gates every
hook, so leaving it compiled in costs nothing.

**Enable:** `THEGN_PERF=1`, or any `THEGN_LOG` that selects `thegn::perf`
(e.g. `THEGN_LOG=thegn::perf=debug`). Also forced on automatically while the
Telemetry panel section is open (§4).

**Emits** a rollup every `THEGN_PERF_INTERVAL_MS` (default 10000), piggy-backed
on an existing wake — never its own timer thread (which would itself be a wake
source). At `info`:

```
thegn::perf  perf rollup  wakes_per_s renders_per_s render_skips_per_s
   render_p50_us render_p99_us idle_ratio hot_source hot_items_per_s
   pty_chunks_per_s pty_budget_hits
   cpu_hydrate_ms cpu_stats_ms cpu_pr_ms cpu_metrics_ms cpu_dashboard_ms cpu_diff_ms
   cpu_process_ms cpu_loop_ms cpu_scan_ms cpu_children_reaped_ms cpu_unattributed_ms
   malloc_arena_max malloc_arena_cap_applied malloc_trims_idle malloc_trims_scan
   malloc_scan_drains malloc_trims_released malloc_trim_bytes malloc_trim_total_us
   malloc_trim_max_us
```

At `debug`, one `perf source` line per active wake source (`Model`, `Watcher`,
`Stats`, …). Wake-source attribution works by counting messages at each of the
~24 channel-drain sites (the `TerminalWaker` carries no reason), and CPU is the
_thread's own_ `CLOCK_THREAD_CPUTIME_ID` charged per subsystem via RAII
`perf::measure(Subsys::…)` guards — so an I/O-blocked git fan-out reports the CPU
it burned, not the wall-clock it waited.

**Wake-storm warning** (`warn`, shows at default level): if the loop is idle
(`idle_ratio > 0.95`) yet pulsing above `THEGN_PERF_WAKE_LIMIT` (default 20/s),
it names the dominant source — exactly the diff-watcher `.git/` storm this
codebase has hit.

Tuning env: `THEGN_PERF_INTERVAL_MS`, `THEGN_PERF_WAKE_LIMIT`.

## 2. Idle / steady-state CPU harness

`test/perf/cpu-sample.sh` (+ `lib/env.sh`, `lib/fixture.sh`). Launches thegn in
a PTY over a hermetic fixture of N worktrees (isolated `HOME`/XDG/gitconfig), lets
it settle, samples `/proc/<pid>/stat` over a window, and reports cores-used with a
per-thread breakdown. Pass `--worktrees N` to size the fixture; the harness
seeds its worktrees into the registry before the measured launch. This is the
steady-state cost `just bench` (launch→first frame only) never sees.

Enabled by a new run hook **`THEGN_BENCH_RUN_MS`**: runs the full loop (ticker,
hydration, tokio pool included) for a fixed window, then exits via the existing
`shutdown` flag + a single waker pulse — honoring the no-poll-timeout invariant.

Recipes:

- `just bench-idle` — asserts idle `cores_total` ≤ a **fixed** ceiling (the 0%-idle
  invariant, finally a test). The ceiling is a constant, not the baseline, so a
  regressed baseline can't raise the bar.
- `just bench-idle-record` — record this machine's baseline (`test/perf/baselines/
<host-tag>.idle.json`; host-tag = arch + cpu-model hash, so machine dependence is
  explicit).
- `just bench-steady` — feeds `test/perf/scenarios/steady-workload.keys`; A/B only.
  Workload keys are paced through the harness PTY; the run fails with
  `FAIL: key injection had no observable effect` if no key dispatch is logged.
  `TG_PERF_DROP_KEYS=1` is the negative control. Result JSON includes `input`
  (null for idle). The environment scrubs inherited `THEGN_*` and credential
  variables while keeping `TG_PERF_*`; soak-daemon binds its own daemon socket.
- The shell perf harness writes `[media] enabled = false` so the media fallback cannot
  spawn `playerctl` when the isolated session has no MPRIS bus. The harness
  disables sandboxing by default; set `TG_PERF_SANDBOX=1` to
  measure with sandboxed panes enabled. Sandbox-on mode never exposes the host
  user manager or session bus. It writes a fixture Podman config with
  `cgroup_manager = "cgroupfs"` and `events_logger = "file"`, and places
  `[containers] cgroups = "disabled"` in the same config so rootless Podman
  does not need a systemd cgroup scope. It also places
  fail-closed `systemd-run` and `systemctl` shims first on `PATH`. The shims
  record argv in `$PERF_TMP/systemd-calls.log`; the harness fails if the log is
  nonempty. The fixture writes
  `cpu_total = "off"` and `memory_total = "off"` to its sandbox limits in
  either mode. A separate verify launch checks that the seeded worktree count
  was collected before the measured launch. The `soak` and `soak-daemon`
  fixtures set `[git] auto_fetch = false` so initial per-worktree remote
  discovery stays outside the measurement window. Fixed resource ceilings
  apply to every non-debug procfs run; the CPU ceiling applies to non-debug
  idle runs.
  Cleanup signals a process group only while its tracked launcher is still a
  child of the harness, and only for groups registered by the harness; it does
  not signal a launcher group after that child has been reaped. Fixture cleanup
  runs `podman unshare rm` before ordinary removal when temporary Podman
  storage exists, then scans `/proc/PID/comm` for `podman pause` (its cmdline
  is only `podman`) and removes matching processes whose environment names the
  fixture path. Procfs read races are ignored. Registry-seeding status goes to
  stderr so `--json` stdout remains a single JSON object.
  Verify JSON stdout with:

  ```sh
  bash test/perf/cpu-sample.sh --scenario soak --worktrees 3 --json 2>/dev/null |
    python3 -c 'import json,sys; json.load(sys.stdin)'
  ```

The fixture (`lib/fixture.sh`) and the Rust bench fixture
(`crates/thegn-svc/benches/support/fixture.rs`) build the same layout — keep
them in sync.

## 3. Criterion micro-benchmarks

- `crates/thegn-svc/benches/git_hot.rs` — the per-worktree git hot path the 2s
  ticker fans across every worktree: `is_dirty` / `ahead_behind` / `current_branch`
  individually and combined ("model scan"), parametrized by worktree count
  (1/4/14), **gix vs CLI** so the provider choice is measured, not folklore.
- `crates/thegn-core/benches/core_hot.rs` — theme/palette construction (startup +
  every theme cycle).

Run: `just bench-micro` (whole workspace) or `just bench-micro-svc` (git only).
Debug-vs-release A/B: `cargo bench` (release-grade bench profile) vs
`cargo bench --profile dev`.

## 4. Live in-app overlay

The Telemetry panel section (System tab → Telemetry) grows a **LOOP** sub-block:
`wakes/s`, `rend/s`, render `p99`, the hot wake source, and idle %, with a braille
graph in the wider layouts. Opening the section forces perf accounting on and rolls
up every 1s (restoring the prior state on close, so a `THEGN_PERF=1` user keeps
accounting). It's the live view of the same data the `thegn::perf` log emits.

## 5. Flame-graph profiling

Two paths, because `ptrace_scope=1` blocks attaching to a running process.

**Primary — in-process** (`crates/thegn-host/src/profile.rs`, behind the
`profiling` cargo feature; zero cost otherwise). `just profile` builds
`--release --features profiling` and launches. `kill -USR2 <pid>` starts sampling,
a second `SIGUSR2` writes a flamegraph SVG to
`$XDG_STATE_HOME/thegn/profiles/`. This is the only way to profile the live
daily multiplexer.

**Secondary — external.** `cargo flamegraph --bin thegn` works because thegn is
then the profiler's _child_ (ptrace permits descendants); wrap in `script` for a
PTY. Attaching to an already-running thegn needs `ptrace_scope=0` (don't depend
on it).

## Guard rails

`just _perf-guard` (run by every CPU recipe) refuses a debug or stale
`target/release/thegn` and prints the resolved binary + mtime + profile, so a
number is never silently taken from the wrong build — the debug-vs-release CPU gap
is ~2.5×. `just perf` is the umbrella: startup + idle + micro.

Timings are machine-dependent, so none of these are in `just ci` (mirroring
`just bench`). A non-gating CI `perf-report` can run `bench-idle` and post
`cores_total` vs the committed baseline as an artifact.
