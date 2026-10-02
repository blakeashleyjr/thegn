#!/usr/bin/env bash
# Steady-state / idle CPU harness for thegn.
#
# Launches thegn inside a PTY (via script(1), like test/pty-smoke.sh) in a
# fully isolated environment with a fixture repo of N worktrees, lets it settle,
# then samples the process's CPU (utime+stime from /proc) over a fixed window
# and reports cores-used with a per-thread breakdown. This finally measures the
# steady-state cost the launch→first-frame `just bench` never sees.
#
# It also samples RESOURCE ACCUMULATION — children (zombies counted
# separately), fds by class, thread high-water and the read-syscall rate. CPU was
# the only axis gated here, and the incident that motivated this consumed **zero**
# CPU in the thing that broke the machine: 4,408 unreaped `git-lfs` children took
# the process table to 5,274 entries and I/O pressure `full avg10` to 7.94, while
# the same instance's cores-used looked unremarkable. Counters that only ever grow
# are invisible to a windowed CPU measurement, so they get their own ceilings.
#
# Usage:
#   cpu-sample.sh [--scenario idle|steady-workload|soak|soak-daemon] [--bin PATH]
#                 [--worktrees N] [--dirty N] [--settle-ms MS] [--window-ms MS]
#                 [--ceiling CORES] [--record] [--json] [--baseline-dir DIR]
#                 [--zombie-ceiling N] [--child-growth-ceiling N]
#                 [--fd-growth-ceiling N] [--thread-growth-ceiling N]
#                 [--syscr-rate-ceiling N]
#
# Exit status: 0 ok; 2 over a ceiling (CPU or resource); 1 harness error.

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Two sampling backends, because the 0%-idle invariant is the hardest contract
# this project has and "unmeasured on that platform" is not a state it should be
# allowed to sit in:
#
#   linux  — /proc/PID/stat + /proc/PID/task/*/stat. Exact, and the only one
#            that can attribute cores to individual THREADS.
#   darwin — `top -l 2 -s <window> -pid`, whose second sample is CPU% averaged
#            over the interval (validated: ~95% for a spinning process, 0.0% for
#            a sleeping one). Same `cores_total` number, no per-thread breakdown:
#            macOS has no per-thread equivalent short of `sample`(1)/DTrace, and
#            neither survives SIP on an unsigned binary.
#
# Anything else still skips rather than failing with "no such file" noise.
case "$(uname -s)" in
Linux) SAMPLER=proc ;;
Darwin) SAMPLER=top ;;
*)
  echo "cpu-sample: no CPU sampler for $(uname -s) — skipping"
  exit 0
  ;;
esac

# shellcheck source=test/perf/lib/env.sh disable=SC1091
source "$HERE/lib/env.sh"
# shellcheck source=test/perf/lib/fixture.sh disable=SC1091
source "$HERE/lib/fixture.sh"
# Portable `script`/`timeout` wrapping — the util-linux vs BSD split.
# shellcheck source=test/lib/pty.sh disable=SC1091
source "$HERE/../lib/pty.sh"

SCENARIO=idle
BIN="${TG_PERF_BIN:-target/release/thegn}"
WORKTREES="${TG_PERF_WORKTREES:-14}"
DIRTY="${TG_PERF_DIRTY:-4}"
SETTLE_MS=2500
WINDOW_MS=8000 # long enough to average the dashboard's 4s sysinfo cadence
# cores; the encoded 0%-idle guard (FIXED, not baseline-derived). Observed idle
# on the 14-worktree fixture (release) is ~0.056 cores, dominated by the
# pre-warmed dashboard collector; a true event-loop spin regression would be
# 0.5-1.5 cores, far past this. Tighten once the dashboard poll is visibility-gated.
CEILING=0.12
# Resource ceilings. FIXED guards like CEILING, never baseline-derived: a
# regressed baseline must not be able to raise the bar on a leak.
#
# A zombie child is a defect by definition — the process exited and nobody called
# `wait()` — so that ceiling is 0 and is not scenario-dependent. The growth
# ceilings are deltas across the sample window on an already-settled process:
# hydration and lazy opens happen during the settle, so a settled window that
# keeps adding children, fds or threads is accumulating, not warming up. They are
# small rather than zero because the ticker and the sysinfo collector legitimately
# open and close things mid-window.
ZOMBIE_CEILING=0
# Live children legitimately fluctuate — measured 0 -> 2 across a 45s idle window
# as the pane/git helpers come and go — so this is a coarse backstop, not the
# leak signal. ZOMBIE_CEILING is the leak signal: a zombie is never legitimate.
CHILD_GROWTH_CEILING=4
# Measured on a 14-worktree fixture: +2 over an 8s window, +1 over 45s — churn,
# not accumulation (a real leak would scale with the window, not shrink).
FD_GROWTH_CEILING=8
THREAD_GROWTH_CEILING=2
# Reads per second. The incident sustained 26,063/s from the disk-size walk.
# Measured idle here: ~900/s over a 45s window, ~2,100/s over 8s — the cost is
# front-loaded just after the settle, so the ceiling has to clear the short-window
# case. 5,000 does, and still catches the incident by 5x.
SYSCR_RATE_CEILING=5000
# Idle process starts are forbidden in the soak window. This ceiling is fixed
# by doctrine, not measured from a baseline, and is deliberately not a CLI knob.
SPAWN_RATE_CEILING=0
# One strace recipe for both processes. Every flag is load-bearing:
#   -f                follow forks (git/podman/... are children of thegn)
#   -qq               silence "Process N attached/exited" tracer chatter
#   --seccomp-bpf     only the filtered syscalls stop; idle cost stays ~zero
#   -ttt              epoch timestamps, comparable with the window's `date +%s.%N`
#   -s 4096           NO string truncation: the default 32-char limit turns the
#                     trampoline's long paths into `"..."...`
#   -e signal=none    `-qq` does NOT suppress `--- SIGCHLD ---` lines; this does
#   -e trace=...      execve = the spawn; clone/clone3/fork/vfork give the
#                     pid->parent map that separates pane-shell subtrees (rc files,
#                     prompt hooks) from thegn's own spawns -- see spawn-trace.py
# KNOWN LIMITATION: ptrace neutralises setuid, so `sudo -n podman` (the rootful
# probe) fails under the tracer, rootful podman is cached Absent, and the run
# under-counts the spawns a production (untraced) thegn makes. Rootless
# `podman ps` / `docker ps` are still seen; a green result here is therefore a
# floor, not proof that no setuid helper is ever spawned.
SPAWN_STRACE_FLAGS="-f -qq --seccomp-bpf -ttt -s 4096 -e signal=none -e trace=execve,clone,clone3,fork,vfork"
RECORD=0
JSON_ONLY=0
BASELINE_DIR="$HERE/baselines"

while [ $# -gt 0 ]; do
  case "$1" in
  --scenario)
    SCENARIO="$2"
    shift 2
    ;;
  --bin)
    BIN="$2"
    shift 2
    ;;
  --worktrees)
    WORKTREES="$2"
    shift 2
    ;;
  --dirty)
    DIRTY="$2"
    shift 2
    ;;
  --settle-ms)
    SETTLE_MS="$2"
    shift 2
    ;;
  --window-ms)
    WINDOW_MS="$2"
    shift 2
    ;;
  --ceiling)
    CEILING="$2"
    shift 2
    ;;
  --zombie-ceiling)
    ZOMBIE_CEILING="$2"
    shift 2
    ;;
  --child-growth-ceiling)
    CHILD_GROWTH_CEILING="$2"
    shift 2
    ;;
  --fd-growth-ceiling)
    FD_GROWTH_CEILING="$2"
    shift 2
    ;;
  --thread-growth-ceiling)
    THREAD_GROWTH_CEILING="$2"
    shift 2
    ;;
  --syscr-rate-ceiling)
    SYSCR_RATE_CEILING="$2"
    shift 2
    ;;
  --record)
    RECORD=1
    shift
    ;;
  --json)
    JSON_ONLY=1
    shift
    ;;
  --baseline-dir)
    BASELINE_DIR="$2"
    shift 2
    ;;
  *)
    echo "cpu-sample: unknown arg: $1" >&2
    exit 1
    ;;
  esac
done

SPAWN_ENABLED=0
SPAWN_JSON="null"
SPAWN_FAIL=0
if [ "$SCENARIO" = soak ] || [ "$SCENARIO" = soak-daemon ]; then
  SPAWN_ENABLED=1
  case "$SAMPLER" in
  top)
    SPAWN_JSON='{"status":"unsupported","method":"unsupported","count":null,"ceiling":0,"offenders":[]}'
    ;;
  proc)
    if ! command -v strace >/dev/null 2>&1; then
      echo 'FAIL: spawn-rate axis requires strace; re-enter nix develop after the devShell change' >&2
      if [ "$JSON_ONLY" = 1 ]; then
        printf '%s\n' '{"spawn_rate":{"status":"failed","method":"strace","count":null,"ceiling":0,"offenders":[],"error":"strace missing"}}'
      fi
      exit 2
    fi
    STRACE_BIN="$(command -v strace)"
    ;;
  esac
fi

# `soak` is `steady-workload` given room to accumulate: the resource counters are
# deltas, so a leak of one child per diff is only visible if enough diffs happen
# and enough worktrees exist to keep the scan lanes busy. Scenario defaults apply
# only where the caller did not ask for a value, so an explicit flag always wins.
if [ "$SCENARIO" = soak ] || [ "$SCENARIO" = soak-daemon ]; then
  [ "$WORKTREES" = "${TG_PERF_WORKTREES:-14}" ] && WORKTREES=40
  [ "$WINDOW_MS" = 8000 ] && WINDOW_MS=60000
fi

BIN_ABS="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN_ABS" ] || {
  echo "cpu-sample: binary not executable: $BIN_ABS" >&2
  exit 1
}
case "$BIN_ABS" in
*target/release/*) BUILD=release ;;
*target/debug/*) BUILD=debug ;;
*) BUILD=unknown ;;
esac

CLK_TCK="$(getconf CLK_TCK 2>/dev/null || echo 100)"
GIT_SHA="$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)"

perf_make_tmp
perf_trap_cleanup
HOST_TAG="$(perf_host_tag)"
REPO="$(perf_build_fixture "$WORKTREES" "$DIRTY")"

command -v script >/dev/null 2>&1 || {
  echo "cpu-sample: script(1) not found" >&2
  exit 1
}

PIDFILE="$PERF_TMP/thegn.pid"
TRACE_UI_FILE="$PERF_TMP/ui.execve"
TRACE_UI_PIDFILE="$PERF_TMP/ui.strace.pid"
# The adaptive settle can extend to SETTLE_CAP_MS (20s), so the run window has to
# cover the cap rather than the requested settle — otherwise thegn exits mid-sample
# on exactly the slow-hydration runs the adaptive settle exists for.
# Total settle is max(SETTLE_MS, 20000): the adaptive loop starts its count at
# SETTLE_MS and only runs while under the 20 s cap, so an explicit --settle-ms
# past the cap is used as-is. Ignoring SETTLE_MS here made any --settle-ms above
# ~20 s exit thegn mid-measurement.
SETTLE_MAX_MS=$((SETTLE_MS > 20000 ? SETTLE_MS : 20000))
# THEGN_BENCH_RUN_MS counts from thegn's OWN start, but the harness's clock also
# carries launch/attach lag, the adaptive settle's whole-second overshoot past the
# cap (it exits at 20.5 s, not 20 s), and per-sample overhead under load. The old
# 1.5 s tail was smaller than that, so thegn self-exited a little before the
# window ended ("thegn exited before the spawn-rate idle window ended") and every
# number from that run under-measured. The harness SIGTERMs thegn when done, so a
# long tail costs nothing; 30 s covers any observed lag with room to spare.
RUN_MS=$((SETTLE_MAX_MS + 1000 + WINDOW_MS + 30000))
DEADLINE_S=$(((RUN_MS / 1000) + 10)) # hard safety net

# Launch thegn under a PTY (termwiz refuses to start without one); the inner
# shell backgrounds thegn and records its PID so the sampler can find it.
# THEGN_BENCH_RUN_MS makes thegn run the full loop — ticker, hydration, tokio
# pool — then exit cleanly on its own. THEGN_NO_DAEMON: the bench-window exit
# detaches daemon panes — each run would strand a never-reaped session (and its
# daemon) in the bench state dir.
#
# Through `pty_run`/`pty_timeout_bin` rather than `script -qec` + `timeout`
# directly: both are util-linux spellings that BSD/macOS rejects outright
# ("illegal option -- e"), which is how this harness reported "thegn did not
# start" on a Mac when the real failure was its own launch line. `just bench`
# already routes through the same helpers.
# `soak-daemon` is the one scenario that LEAVES the daemon enabled, because the
# daemon is the process the other scenarios structurally cannot see — and it is
# the one that lives for days, so a per-event leak there is the worst kind. It
# is safe to enable only because `perf_make_tmp` now isolates XDG_RUNTIME_DIR;
# without that the daemon socket resolves into the developer's real
# /run/user/<uid> and this would attach to their live session's daemon.
if [ "$SCENARIO" = soak-daemon ]; then
  NO_DAEMON=""
else
  NO_DAEMON="THEGN_NO_DAEMON=1"
fi
if [ "$SPAWN_ENABLED" = 1 ] && [ "$SAMPLER" = proc ]; then
  # `$1`/`$@` are the bash -c positionals passed after `$0`, not outer values.
  # shellcheck disable=SC2016
  printf -v INNER \
    'cd %q; stty rows 50 cols 200; env THEGN_BENCH_RUN_MS=%q %s %q %s -o %q bash -c '\''echo $$ > "$1"; shift; exec "$@"'\'' _ %q %q & echo $! > %q; wait' \
    "$REPO" "$RUN_MS" "$NO_DAEMON" "$STRACE_BIN" "$SPAWN_STRACE_FLAGS" "$TRACE_UI_FILE" "$PIDFILE" "$BIN_ABS" "$TRACE_UI_PIDFILE"
else
  printf -v INNER \
    'cd %q; stty rows 50 cols 200; env THEGN_BENCH_RUN_MS=%q %s %q & echo $! > %q; wait' \
    "$REPO" "$RUN_MS" "$NO_DAEMON" "$BIN_ABS" "$PIDFILE"
fi
TIMEOUT_BIN="$(pty_timeout_bin)"
# Single quotes are deliberate: $0/$1 are the INNER bash's positionals, bound by
# the two arguments below, not this shell's. (Same idiom as flood.sh.)
# shellcheck disable=SC2016
if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "${DEADLINE_S}s" bash -c 'source "$0"; pty_run "$1"' \
    "$HERE/../lib/pty.sh" "$INNER" >/dev/null 2>&1 &
else
  # No timeout binary: the bench window (THEGN_BENCH_RUN_MS) still exits on its
  # own, so we lose only the belt-and-braces deadline.
  bash -c 'source "$0"; pty_run "$1"' "$HERE/../lib/pty.sh" "$INNER" >/dev/null 2>&1 &
fi
LAUNCHER=$!

# `soak-daemon` starts the daemon ITSELF rather than waiting for the UI to spawn
# one lazily.
#
# The lazy path needs a pane attach, and attaching a pane in this headless
# fixture is not solved: the shared workload keystrokes are sidebar navigation
# only, and `new-pane` needs UI context this harness does not set up. Feeding
# keys until something opens would be tuning a gate until it passes, which is
# how you get one that measures nothing.
#
# So the scope is stated instead of fudged. This scenario watches the DAEMON
# PROCESS — its fds, threads, children and zombies over a settled window — which
# is real coverage of the longest-lived process thegn runs, and coverage no other
# scenario has. It does NOT exercise pane teardown; the specific regression there
# (THE-704, a pane child left unreaped when its consumer goes away) is pinned by
# `pane_pty::tests::a_pane_whose_consumer_vanishes_leaves_no_child_behind`, which
# has a verified failing control. Wiring headless pane sessions in here would
# extend this scenario to that path too.
DAEMON_LAUNCHER=""
if [ "$SCENARIO" = soak-daemon ]; then
  # `--socket` explicitly, even though the env already resolves to this path:
  # `find_daemon_pid` matches on the isolated runtime dir appearing in ARGV, so
  # that it can never pick up the developer's live daemon. A daemon started
  # without the flag inherits the path from the environment and is invisible to
  # that match — which is exactly how this scenario first reported "not found"
  # while its own daemon was running.
  mkdir -p "$XDG_RUNTIME_DIR/thegn"
  if [ "$SPAWN_ENABLED" = 1 ] && [ "$SAMPLER" = proc ]; then
    TRACE_DAEMON_FILE="$PERF_TMP/daemon.execve"
    DAEMON_PIDFILE="$PERF_TMP/daemon.pid"
    # The tracer's argv ALSO contains the isolated socket path, so matching argv
    # (find_daemon_pid) would pick up strace -- and strace has 1 child, 0 zombies
    # and 1 thread, passing every ceiling the daemon exists to be held to. Same
    # trampoline as the UI: the daemon records its OWN pid, then exec()s into it.
    # shellcheck disable=SC2016,SC2086
    "$STRACE_BIN" $SPAWN_STRACE_FLAGS -o "$TRACE_DAEMON_FILE" \
      bash -c 'echo $$ > "$1"; shift; exec "$@"' _ "$DAEMON_PIDFILE" \
      "$BIN_ABS" daemon --socket "$XDG_RUNTIME_DIR/thegn/daemon.sock" >/dev/null 2>&1 &
  else
    DAEMON_PIDFILE=""
    "$BIN_ABS" daemon --socket "$XDG_RUNTIME_DIR/thegn/daemon.sock" >/dev/null 2>&1 &
  fi
  DAEMON_LAUNCHER=$!
  # The socket is the readiness signal; the UI attaches to the same path.
  for _ in $(seq 1 100); do
    [ -S "$XDG_RUNTIME_DIR/thegn/daemon.sock" ] && break
    sleep 0.1
  done
fi

# Wait for the PID file (thegn up).
for _ in $(seq 1 100); do
  [ -s "$PIDFILE" ] && break
  sleep 0.05
done
PID="$(cat "$PIDFILE" 2>/dev/null || true)"
if [ -z "$PID" ] || ! kill -0 "$PID" 2>/dev/null; then
  if [ "$SPAWN_ENABLED" = 1 ] && [ "$SAMPLER" = proc ]; then
    echo 'FAIL: spawn-rate UI tracer failed to start thegn' >&2
    if [ "$JSON_ONLY" = 1 ]; then
      printf '%s\n' '{"spawn_rate":{"status":"failed","method":"strace","count":null,"ceiling":0,"offenders":[],"roots":1,"error":"tracer failed to start target"}}'
    fi
  else
    echo "cpu-sample: thegn did not start" >&2
  fi
  kill "$LAUNCHER" 2>/dev/null || true
  [ "$SPAWN_ENABLED" = 1 ] && exit 2
  exit 1
fi

# /proc/<pid>/stat fields 14,15 = utime,stime (in CLK_TCK). The comm field (2)
# may contain spaces/parens, so split on the LAST ')'.
proc_jiffies() { # $1 = pid -> utime+stime
  awk '{ s=$0; sub(/^.*\) /,"",s); split(s,a," "); print a[12]+a[13] }' "/proc/$1/stat" 2>/dev/null || echo 0
}

proc_running() { # true only for a live, non-zombie process
  local state
  kill -0 "$1" 2>/dev/null || return 1
  state="$(awk '{ s=$0; sub(/^.*\) /,"",s); split(s,a," "); print a[1] }' "/proc/$1/stat" 2>/dev/null || true)"
  [ -n "$state" ] && [ "$state" != Z ]
}

# --- resource accumulation --------------------------------------------------
# One awk pass over the whole process table rather than a `cat` per pid: this runs
# with ~900 processes live and is called at both ends of the window.
#
# `sub(/^[0-9]+ \(.*\) /)` is deliberately greedy — a comm can contain both spaces
# and parens, so only the LAST ") " is the real delimiter. After it, field 1 is the
# state and field 2 the ppid.
proc_children() { # $1 = pid -> "children zombies"
  awk -v target="$1" '
    FNR == 1 {
      s = $0; sub(/^[0-9]+ \(.*\) /, "", s); split(s, a, " ")
      if (a[2] == target) { c++; if (a[1] == "Z") z++ }
    }
    END { printf "%d %d", c + 0, z + 0 }
  ' /proc/[0-9]*/stat 2>/dev/null || printf '0 0'
}

# fds by class, because "47 fds" says nothing about which subsystem is leaking.
# `thegn.db` is called out by name: the incident held 64 connections to it.
proc_fds() { # $1 = pid -> "total db procfs sock pipe"
  local total=0 db=0 pr=0 sk=0 pi=0 link target
  for link in "/proc/$1/fd"/*; do
    [ -e "$link" ] || continue # glob stays literal if the process vanished
    total=$((total + 1))
    target="$(readlink "$link" 2>/dev/null)" || continue
    case "$target" in
    *thegn.db*) db=$((db + 1)) ;;
    /proc/*) pr=$((pr + 1)) ;;
    socket:*) sk=$((sk + 1)) ;;
    pipe:*) pi=$((pi + 1)) ;;
    esac
  done
  printf '%d %d %d %d %d' "$total" "$db" "$pr" "$sk" "$pi"
}

proc_threads() { # $1 = pid -> live thread count
  local n=0 d
  for d in "/proc/$1/task"/*; do
    [ -e "$d" ] || continue
    n=$((n + 1))
  done
  printf '%d' "$n"
}

# Read-syscall COUNT, not bytes. The runaway instance had made 4.5 TB of `rchar`
# against 0.6 MB/s of real disk — i.e. it was re-reading the page cache, which
# shows up in the syscall count and not in any I/O-bytes metric.
proc_syscr() { # $1 = pid -> cumulative read syscalls
  awk '/^syscr:/ { print $2; found = 1 } END { if (!found) print 0 }' "/proc/$1/io" 2>/dev/null || echo 0
}

# The pane daemon this run started, or empty.
#
# Matched on OUR isolated runtime dir appearing in the process's argv — not on
# the name `thegn daemon`, which would also match the developer's live daemon
# and turn this gate into a reading of whatever their session happens to be
# doing. `perf_make_tmp` makes `$XDG_RUNTIME_DIR` unique per run, and the UI
# passes the resolved socket path to the daemon it spawns, so the match is exact.
find_daemon_pid() {
  local d
  for d in /proc/[0-9]*; do
    [ -r "$d/cmdline" ] || continue
    # NUL-separated argv; translate so a plain grep works.
    if tr '\0' ' ' <"$d/cmdline" 2>/dev/null | grep -q -- "$XDG_RUNTIME_DIR"; then
      printf '%s' "${d##*/}"
      return 0
    fi
  done
  return 1
}

res_sample() { # $1 = pid -> "children zombies fdtotal fddb fdproc fdsock fdpipe threads syscr"
  printf '%s %s %s %s' \
    "$(proc_children "$1")" "$(proc_fds "$1")" "$(proc_threads "$1")" "$(proc_syscr "$1")"
}

if [ "$SCENARIO" = steady-workload ] || [ "$SCENARIO" = soak ] || [ "$SCENARIO" = soak-daemon ]; then
  # `soak-daemon` needs its own keystrokes. The shared workload file is sidebar
  # navigation only — arrows and a Tab — so it never attaches a pane, and the
  # daemon is spawned LAZILY on the first attach. With the navigation keys the
  # scenario found no daemon and (correctly) failed rather than reporting a
  # clean run. These open panes instead, and leave them open so the daemon has
  # live sessions to hold for the whole sample window.
  KEYS="$HERE/scenarios/steady-workload.keys"
  [ "$SCENARIO" = soak-daemon ] && KEYS="$HERE/scenarios/soak-daemon.keys"
  # The pty master is only reachable through /proc on Linux; on darwin the
  # scenario degrades to plain idle rather than silently claiming a workload.
  if [ "$SAMPLER" = proc ] && [ -f "$KEYS" ]; then
    cat "$KEYS" >"/proc/$PID/fd/0" 2>/dev/null || true
  elif [ "$SAMPLER" != proc ]; then
    echo "cpu-sample: steady-workload keystrokes need /proc — measuring idle instead" >&2
  fi
fi

WINDOW_S="$(awk "BEGIN{print $WINDOW_MS/1000}")"
THREAD_JSON="[]"
THREAD_TABLE=""
# Absent on darwin: none of these counters has a procfs-free equivalent, and
# reporting an invented zero would read as "no leak" rather than "not measured".
RES_JSON="null"

if [ "$SAMPLER" = proc ]; then
  # Settle, then capture the process + per-thread baseline at the SAME instant
  # (window start), sleep the window, and diff.
  sleep "$(awk "BEGIN{print $SETTLE_MS/1000}")"

  # ADAPTIVE SETTLE. A fixed 2500ms was not enough and it made the repo's hardest
  # contract report a false failure: on this box, same binary and fixture,
  # cores_total measured 0.334 at settle=2500ms, 0.068 at 6000ms and 0.035 at
  # 12000ms — the last better than the recorded 0.0488 baseline. The cost is
  # unfinished hydration landing inside the window, not idle spin, and a loaded
  # box makes hydration longer so more of it lands there.
  #
  # So wait for quiescence instead of guessing it: poll one-second CPU deltas
  # until one comes in under half the ceiling. Bounded, because a genuine spin
  # would never quiesce and must still be measured rather than waited on forever.
  #
  # `settle_used_ms` is REPORTED, so hydration getting slower stays visible
  # instead of being silently absorbed — this separates "idle costs too much"
  # (this gate) from "startup takes too long" (`just bench`), which is the split
  # the fixed settle was conflating.
  SETTLE_CAP_MS=20000
  QUIESCENT_CORES="$(awk "BEGIN{print $CEILING/2}")"
  SETTLE_USED_MS="$SETTLE_MS"
  while [ "$SETTLE_USED_MS" -lt "$SETTLE_CAP_MS" ]; do
    Q0="$(proc_jiffies "$PID")"
    sleep 1
    Q1="$(proc_jiffies "$PID")"
    SETTLE_USED_MS=$((SETTLE_USED_MS + 1000))
    awk "BEGIN{exit !(($Q1-$Q0)/$CLK_TCK <= $QUIESCENT_CORES)}" && break
  done

  J0="$(proc_jiffies "$PID")"
  # Resources at window start — AFTER the settle, so hydration and the lazy opens
  # it triggers are already accounted and any growth from here is accumulation.
  read -r C0 Z0 FD0 FDDB0 FDPR0 FDSK0 FDPI0 TH0 SR0 <<<"$(res_sample "$PID")"
  # The daemon, when this scenario started one. Sampled on the SAME axes: it is
  # the longest-lived process thegn runs, so it is the one where a per-event
  # leak compounds — and the one every other scenario is blind to.
  if [ -n "${DAEMON_PIDFILE:-}" ]; then
    DPID="$(cat "$DAEMON_PIDFILE" 2>/dev/null || true)"
  else
    DPID="$(find_daemon_pid || true)"
  fi
  # Never sample the wrong process. A tracer wrapper (or anything that is not the
  # thegn binary) here would make every daemon ceiling vacuous.
  if [ -n "$DPID" ]; then
    DCOMM="$(cat "/proc/$DPID/comm" 2>/dev/null || true)"
    if [ "$DCOMM" = strace ] || [ "$DCOMM" != "$(basename "$BIN_ABS" | cut -c1-15)" ]; then
      echo "FAIL: soak-daemon would sample pid $DPID comm='$DCOMM', not the thegn daemon" >&2
      DAEMON_WRONG_PID=1
    fi
  fi
  if [ -n "$DPID" ]; then
    read -r DC0 DZ0 DFD0 _ _ _ _ DTH0 _ <<<"$(res_sample "$DPID")"
  fi
  # Per-thread CPU **and** per-thread reads. Both, because they answer different
  # questions and the second one is what settled this investigation: the
  # process-wide counter read 5,000-12,000 reads/s and looked alarming, while
  # per-thread showed 3.6/s on the event loop — the no-blocking-I/O invariant
  # intact — and named the real consumer. Attribution needs the per-thread file;
  # total cost needs the process one, because the process counter RETAINS the
  # reads of threads that have since exited (which is also why the two never
  # reconcile, and why neither alone is enough).
  declare -A T0 TN TR0
  for tid_dir in "/proc/$PID/task"/*; do
    [ -e "$tid_dir" ] || continue # glob stays literal if the process vanished
    tid="${tid_dir##*/}"
    T0[$tid]="$(awk '{ s=$0; sub(/^.*\) /,"",s); split(s,a," "); print a[12]+a[13] }' "/proc/$PID/task/$tid/stat" 2>/dev/null || echo 0)"
    TN[$tid]="$(cat "/proc/$PID/task/$tid/comm" 2>/dev/null || echo '?')"
    TR0[$tid]="$(awk '/^syscr:/ { print $2 }' "/proc/$PID/task/$tid/io" 2>/dev/null || echo 0)"
  done
  if [ "$SPAWN_ENABLED" = 1 ]; then
    TRACE_UI_PID="$(cat "$TRACE_UI_PIDFILE" 2>/dev/null || true)"
    if [ -z "$TRACE_UI_PID" ] || ! proc_running "$TRACE_UI_PID"; then
      echo 'FAIL: spawn-rate UI strace exited before the idle window' >&2
      SPAWN_FAIL=1
    fi
    if [ "$SCENARIO" = soak-daemon ] && ! proc_running "${DAEMON_LAUNCHER:-0}"; then
      echo 'FAIL: spawn-rate daemon strace exited before the idle window' >&2
      SPAWN_FAIL=1
    fi
    SPAWN_START_EPOCH="$(date +%s.%N)"
  fi
  sleep "$WINDOW_S"
  # A process that died inside the window measured nothing: its counters read
  # back as zero/negative and would pass every ceiling. Fail every scenario, not
  # only the spawn-rate one.
  if ! proc_running "$PID"; then
    echo 'FAIL: thegn exited before the sample window ended — the numbers below are not a measurement' >&2
    SPAWN_FAIL=1
    RES_FAIL=1
  fi
  if [ "$SPAWN_ENABLED" = 1 ]; then
    SPAWN_END_EPOCH="$(date +%s.%N)"
    if ! proc_running "$PID"; then
      echo 'FAIL: thegn exited before the spawn-rate idle window ended' >&2
      SPAWN_FAIL=1
    fi
    if [ -z "${TRACE_UI_PID:-}" ] || ! proc_running "$TRACE_UI_PID"; then
      echo 'FAIL: spawn-rate UI strace exited before the idle window ended' >&2
      SPAWN_FAIL=1
    fi
    if [ "$SCENARIO" = soak-daemon ] && ! proc_running "${DAEMON_LAUNCHER:-0}"; then
      echo 'FAIL: spawn-rate daemon strace exited before the idle window ended' >&2
      SPAWN_FAIL=1
    fi
  fi
  J1="$(proc_jiffies "$PID")"
  read -r C1 Z1 FD1 FDDB1 FDPR1 FDSK1 FDPI1 TH1 SR1 <<<"$(res_sample "$PID")"
  if [ -n "${DPID:-}" ] && [ -d "/proc/$DPID" ]; then
    read -r DC1 DZ1 DFD1 _ _ _ _ DTH1 _ <<<"$(res_sample "$DPID")"
  fi
  CORES_TOTAL="$(awk "BEGIN{printf \"%.4f\", ($J1-$J0)/($CLK_TCK*$WINDOW_S)}")"
  SYSCR_RATE="$(awk "BEGIN{printf \"%.1f\", ($SR1-$SR0)/$WINDOW_S}")"
  # High-water, not end-of-window: a child reaped just before the second sample
  # still happened, and the peak is what sizes the process table.
  CHILD_PEAK=$((C1 > C0 ? C1 : C0))
  ZOMBIE_PEAK=$((Z1 > Z0 ? Z1 : Z0))
  FD_PEAK=$((FD1 > FD0 ? FD1 : FD0))
  THREAD_PEAK=$((TH1 > TH0 ? TH1 : TH0))
  CHILD_GROWTH=$((C1 - C0))
  FD_GROWTH=$((FD1 - FD0))
  THREAD_GROWTH=$((TH1 - TH0))
  RES_JSON="{\"children\":{\"start\":$C0,\"end\":$C1,\"peak\":$CHILD_PEAK},\
\"zombies\":{\"start\":$Z0,\"end\":$Z1,\"peak\":$ZOMBIE_PEAK},\
\"fds\":{\"start\":$FD0,\"end\":$FD1,\"peak\":$FD_PEAK,\
\"db\":{\"start\":$FDDB0,\"end\":$FDDB1},\"procfs\":{\"start\":$FDPR0,\"end\":$FDPR1},\
\"sock\":{\"start\":$FDSK0,\"end\":$FDSK1},\"pipe\":{\"start\":$FDPI0,\"end\":$FDPI1}},\
\"threads\":{\"start\":$TH0,\"end\":$TH1,\"peak\":$THREAD_PEAK},\
\"syscr_per_s\":$SYSCR_RATE}"

  # Per-thread deltas. Capture a sorted "comm cores" table for display and a JSON
  # array for the result. Read t1 BEFORE thegn exits (we're still inside the window
  # tail). Done set -e-safe — a vanished tid just contributes nothing.
  THREAD_JSON=""
  READ_TABLE=""
  LOOP_READS_PER_S=0
  for tid in "${!T0[@]}"; do
    t1="$(awk '{ s=$0; sub(/^.*\) /,"",s); split(s,a," "); print a[12]+a[13] }' "/proc/$PID/task/$tid/stat" 2>/dev/null || true)"
    [ -n "$t1" ] || t1="${T0[$tid]}"
    r1="$(awk '/^syscr:/ { print $2 }' "/proc/$PID/task/$tid/io" 2>/dev/null || true)"
    [ -n "$r1" ] || r1="${TR0[$tid]}"
    dr=$((r1 - ${TR0[$tid]}))
    rps="$(awk "BEGIN{printf \"%.1f\", $dr/$WINDOW_S}")"
    # A thread whose tid equals the pid is the process's FIRST thread — the
    # render/input loop. Called out by name because "no blocking I/O on the
    # loop" is a hard invariant, and this is the one number that checks it.
    if [ "$tid" = "$PID" ]; then
      LOOP_READS_PER_S="$rps"
    fi
    [ "$dr" -gt 0 ] && READ_TABLE="$READ_TABLE$rps ${TN[$tid]} (tid $tid)"$'\n'
    dj=$((t1 - ${T0[$tid]}))
    [ "$dj" -gt 0 ] || continue
    c="$(awk "BEGIN{printf \"%.4f\", $dj/($CLK_TCK*$WINDOW_S)}")"
    THREAD_JSON="$THREAD_JSON{\"tid\":$tid,\"comm\":\"${TN[$tid]}\",\"cores\":$c,\"reads_per_s\":$rps},"
    THREAD_TABLE="$THREAD_TABLE$c ${TN[$tid]} (tid $tid)"$'\n'
  done
  THREAD_JSON="[${THREAD_JSON%,}]"
else
  # darwin: `top -l 2 -s <window>` prints two samples; the SECOND is CPU%
  # averaged over the interval between them, which is the windowed measurement
  # we want (the first is a since-launch average and must be discarded — taking
  # it would fold startup into a steady-state number). The settle happens before
  # top starts, so the interval covers only settled time.
  #
  # /usr/bin/top explicitly: nix profiles on macOS can shadow `top` with procps'
  # Linux build, which has neither `-l` nor these `-stats`.
  sleep "$(awk "BEGIN{print $SETTLE_MS/1000}")"
  PCT="$(/usr/bin/top -l 2 -s "$(awk "BEGIN{printf \"%d\", ($WINDOW_S + 0.5)}")" \
    -pid "$PID" -stats cpu 2>/dev/null |
    awk '/^%CPU/ { seen = 1; next } seen && NF { last = $1 } END { print last }')"
  [ -n "$PCT" ] || PCT=0
  CORES_TOTAL="$(awk "BEGIN{printf \"%.4f\", $PCT/100}")"
fi

# Every measurement is taken. Stop thegn now instead of waiting out the bench
# window's (deliberately generous) tail, then reap the launcher.
if [ "$SAMPLER" = proc ] && [ -n "${PID:-}" ]; then
  kill "$PID" 2>/dev/null || true
fi
wait "$LAUNCHER" 2>/dev/null || true
# Stop the daemon this run started. It lives in an isolated state dir, so a
# leaked one would not corrupt anything — but it WOULD sit on a socket under a
# temp dir the EXIT trap is about to delete, and outlive every future run.
if [ -n "${DAEMON_LAUNCHER:-}" ]; then
  "$BIN_ABS" daemon stop >/dev/null 2>&1 || kill "$DAEMON_LAUNCHER" 2>/dev/null || true
  wait "$DAEMON_LAUNCHER" 2>/dev/null || true
fi

if [ "$SPAWN_ENABLED" = 1 ] && [ "$SAMPLER" = proc ]; then
  TRACE_FILES=("$TRACE_UI_FILE")
  if [ "$SCENARIO" = soak-daemon ]; then
    TRACE_FILES+=("$TRACE_DAEMON_FILE")
  fi
  # Opt-in forensics (default behaviour unchanged): TG_PERF_KEEP_TRACE=1 keeps the
  # raw strace files, the window epochs, a full-argv grouping (complete argv,
  # fixture paths normalised, parent process per group) and the isolated state
  # dir (DB + config) in TG_PERF_KEEP_DIR (default $TMPDIR/thegn-perf-trace).
  if [ "${TG_PERF_KEEP_TRACE:-}" = 1 ]; then
    KEEP_DIR="${TG_PERF_KEEP_DIR:-${TMPDIR:-/tmp}/thegn-perf-trace}"
    mkdir -p "$KEEP_DIR"
    cp "${TRACE_FILES[@]}" "$KEEP_DIR/"
    printf '{"start":%s,"end":%s}\n' "$SPAWN_START_EPOCH" "$SPAWN_END_EPOCH" >"$KEEP_DIR/window.json"
    python3 "$HERE/lib/spawn-trace.py" --full-argv --pane-shell "$PERF_PANE_SHELL" "$SPAWN_START_EPOCH" "$SPAWN_END_EPOCH" "${TRACE_FILES[@]}" >"$KEEP_DIR/full-argv.json" 2>"$KEEP_DIR/full-argv.err" || true # best-effort: forensics only
    cp -r "$XDG_STATE_HOME" "$KEEP_DIR/state" 2>/dev/null || true                                                                                                                                              # best-effort: forensics only
    cp -r "$XDG_CONFIG_HOME" "$KEEP_DIR/config" 2>/dev/null || true                                                                                                                                            # best-effort: forensics only
    echo "kept spawn trace + full-argv grouping in $KEEP_DIR" >&2
  fi
  if SPAWN_RESULT="$(python3 "$HERE/lib/spawn-trace.py" --pane-shell "$PERF_PANE_SHELL" "$SPAWN_START_EPOCH" "$SPAWN_END_EPOCH" "${TRACE_FILES[@]}" 2>"$PERF_TMP/spawn-trace.err")"; then
    SPAWN_JSON="${SPAWN_RESULT%\}} ,\"roots\":${#TRACE_FILES[@]}}"
    SPAWN_COUNT="$(printf '%s' "$SPAWN_RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])')"
    if [ "$BUILD" = release ] && [ "$SPAWN_COUNT" -gt "$SPAWN_RATE_CEILING" ]; then
      echo "FAIL: spawn_rate=$SPAWN_COUNT exceeds fixed ceiling=$SPAWN_RATE_CEILING" >&2
      SPAWN_FAIL=1
      SPAWN_JSON="$(printf '%s' "$SPAWN_JSON" | python3 -c 'import json,sys; d=json.load(sys.stdin); d["status"]="failed"; d["error"]="ceiling exceeded"; print(json.dumps(d,separators=(",",":")))')"
    fi
  else
    SPAWN_ERROR="$(cat "$PERF_TMP/spawn-trace.err")"
    echo "FAIL: spawn-rate trace invalid: ${SPAWN_ERROR:-unknown parser failure}" >&2
    # The temp dir is deleted on exit; keep the evidence a parser failure needs.
    for f in "${TRACE_FILES[@]}"; do
      cp "$f" "${TMPDIR:-/tmp}/thegn-invalid-$(basename "$f")" 2>/dev/null &&
        echo "  kept invalid trace: ${TMPDIR:-/tmp}/thegn-invalid-$(basename "$f")" >&2
    done
    SPAWN_JSON='{"status":"failed","method":"strace","count":null,"ceiling":0,"offenders":[],"roots":1,"error":"trace invalid"}'
    SPAWN_FAIL=1
  fi
  if [ "$SPAWN_FAIL" != 0 ]; then
    SPAWN_JSON="$(printf '%s' "$SPAWN_JSON" | python3 -c 'import json,sys; d=json.load(sys.stdin); d["status"]="failed"; d["roots"]=d.get("roots", int(sys.argv[1])); d.setdefault("error", "tracer or target exited before window end"); print(json.dumps(d,separators=(",",":")))' "${#TRACE_FILES[@]}")"
  fi
fi

RESULT="{\"scenario\":\"$SCENARIO\",\"build\":\"$BUILD\",\"worktrees\":$WORKTREES,\"window_ms\":$WINDOW_MS,\"settle_used_ms\":${SETTLE_USED_MS:-$SETTLE_MS},\"cores_total\":$CORES_TOTAL,\"threads\":$THREAD_JSON,\"resources\":$RES_JSON,\"spawn_rate\":$SPAWN_JSON,\"git_sha\":\"$GIT_SHA\",\"host_tag\":\"$HOST_TAG\"}"

BASELINE="$BASELINE_DIR/$HOST_TAG.$SCENARIO.json"
if [ "$RECORD" = 1 ]; then
  mkdir -p "$BASELINE_DIR"
  printf '%s\n' "$RESULT" >"$BASELINE"
fi

if [ "$JSON_ONLY" = 1 ]; then
  printf '%s\n' "$RESULT"
else
  echo "scenario=$SCENARIO build=$BUILD worktrees=$WORKTREES window=${WINDOW_MS}ms settle=${SETTLE_USED_MS:-$SETTLE_MS}ms"
  echo "binary=$BIN_ABS"
  echo "cores_total=$CORES_TOTAL  (host=$HOST_TAG sha=$GIT_SHA)"
  echo "top threads (cores comm tid):"
  if [ -n "$THREAD_TABLE" ]; then
    printf '%s' "$THREAD_TABLE" | sort -rn | head -8 | sed 's/^/  /'
  else
    echo "  (no per-thread CPU captured)"
  fi
  if [ -n "${READ_TABLE:-}" ]; then
    echo "top threads by reads/s (the process total also counts threads that have since exited):"
    printf '%s' "$READ_TABLE" | sort -rn | head -6 | sed 's/^/  /'
    echo "  event loop (tid $PID): ${LOOP_READS_PER_S}/s"
  fi
  if [ "$RES_JSON" != "null" ]; then
    echo "resources (start -> end, peak):"
    echo "  children=$C0 -> $C1 (peak $CHILD_PEAK)   of which zombies=$Z0 -> $Z1 (peak $ZOMBIE_PEAK)"
    echo "  fds=$FD0 -> $FD1 (peak $FD_PEAK)"
    echo "    db=$FDDB0 -> $FDDB1   procfs=$FDPR0 -> $FDPR1   sock=$FDSK0 -> $FDSK1   pipe=$FDPI0 -> $FDPI1"
    echo "  threads=$TH0 -> $TH1 (peak $THREAD_PEAK)"
    echo "  read syscalls=${SYSCR_RATE}/s"
    if [ -n "${DPID:-}" ]; then
      echo "  daemon pid=$DPID comm=${DCOMM:-?}: children=${DC0:-?} -> ${DC1:-?}   zombies=${DZ0:-?} -> ${DZ1:-?}   fds=${DFD0:-?} -> ${DFD1:-?}   threads=${DTH0:-?} -> ${DTH1:-?}"
    elif [ "$SCENARIO" = soak-daemon ]; then
      echo "  daemon: NOT FOUND (see the check below)"
    fi
  else
    echo "resources: not measured on $(uname -s)"
  fi
  if [ "$SPAWN_ENABLED" = 1 ]; then
    if [ "$SAMPLER" = top ]; then
      echo 'spawn rate: unsupported on macOS (no Linux strace axis in this lane)'
    elif [ "$SPAWN_JSON" != 'null' ]; then
      echo "spawn rate: status=$(printf '%s' "$SPAWN_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])') method=strace count=${SPAWN_COUNT:-unavailable} ceiling=$SPAWN_RATE_CEILING roots=${#TRACE_FILES[@]}"
      printf '%s' "$SPAWN_JSON" | python3 -c 'import json,sys; d=json.load(sys.stdin); [print("  %s x%d" % (x["command"],x["count"])) for x in d.get("offenders",[])]; [print("  (pane subtree, not counted) %s x%d" % (x["command"],x["count"])) for x in d.get("excluded",[])]'
    fi
  fi
  if [ -f "$BASELINE" ]; then
    BASE_CORES="$(grep -o '"cores_total":[0-9.]*' "$BASELINE" | cut -d: -f2)"
    echo "baseline=$BASE_CORES  delta=$(awk "BEGIN{printf \"%+.4f\", $CORES_TOTAL-$BASE_CORES}")"
  fi
fi

# The idle scenario encodes the 0%-idle invariant against a FIXED ceiling so a
# regressed baseline can never silently raise the bar.
if [ "$SCENARIO" = idle ] && [ "$BUILD" = release ]; then
  if awk "BEGIN{exit !($CORES_TOTAL > $CEILING)}"; then
    echo "FAIL: idle cores_total=$CORES_TOTAL exceeds ceiling=$CEILING cores" >&2
    exit 2
  fi
fi

# Resource ceilings. These assert on every procfs scenario, not just `idle`: a
# leak under a workload is still a leak, and `steady-workload` is where the diff
# path — the one that leaked 4,408 children — actually runs.
if [ "$RES_JSON" != "null" ] && [ "$BUILD" = release ]; then
  RES_FAIL=0
  res_check() { # $1 = label, $2 = measured, $3 = ceiling, $4 = why it matters
    if [ "$2" -gt "$3" ]; then
      echo "FAIL: $1=$2 exceeds ceiling=$3 — $4" >&2
      RES_FAIL=1
    fi
  }
  res_check zombie_peak "$ZOMBIE_PEAK" "$ZOMBIE_CEILING" \
    "a zombie child exited and nobody called wait() (see THE-701, THE-702)"
  res_check child_growth "$CHILD_GROWTH" "$CHILD_GROWTH_CEILING" \
    "children accumulating across a settled window"
  res_check fd_growth "$FD_GROWTH" "$FD_GROWTH_CEILING" \
    "descriptors accumulating across a settled window (db $FDDB0->$FDDB1, procfs $FDPR0->$FDPR1, sock $FDSK0->$FDSK1, pipe $FDPI0->$FDPI1)"
  res_check thread_growth "$THREAD_GROWTH" "$THREAD_GROWTH_CEILING" \
    "threads accumulating across a settled window (THE-448 leaks one per launch)"
  if awk "BEGIN{exit !($SYSCR_RATE > $SYSCR_RATE_CEILING)}"; then
    echo "FAIL: syscr_per_s=$SYSCR_RATE exceeds ceiling=$SYSCR_RATE_CEILING — a scan lane is probably saturated" >&2
    RES_FAIL=1
  fi
  # The daemon, on the same axes. A zombie here is THE-704's signature: the pane
  # reader used to skip its reap when the consumer went away, so a UI detach left
  # the pane's shell unwaited for the life of the daemon.
  if [ -n "${DPID:-}" ] && [ -n "${DC1:-}" ]; then
    res_check daemon_zombies "${DZ1:-0}" "$ZOMBIE_CEILING" \
      "the pane daemon left a child unreaped (THE-704)"
    res_check daemon_child_growth "$((${DC1:-0} - ${DC0:-0}))" "$CHILD_GROWTH_CEILING" \
      "daemon children accumulating across a settled window"
    res_check daemon_fd_growth "$((${DFD1:-0} - ${DFD0:-0}))" "$FD_GROWTH_CEILING" \
      "daemon descriptors accumulating across a settled window"
    res_check daemon_thread_growth "$((${DTH1:-0} - ${DTH0:-0}))" "$THREAD_GROWTH_CEILING" \
      "daemon threads accumulating across a settled window"
  elif [ "$SCENARIO" = soak-daemon ]; then
    # A gate that silently measures nothing is worse than no gate: this scenario
    # exists ONLY to watch the daemon, so not finding one is a failure of the
    # harness, and saying so beats reporting a clean run.
    echo "FAIL: soak-daemon found no daemon to sample — the scenario measured nothing" >&2
    RES_FAIL=1
  fi
  [ "$SPAWN_FAIL" = 0 ] || RES_FAIL=1
  [ "${DAEMON_WRONG_PID:-0}" = 0 ] || RES_FAIL=1
  [ "$RES_FAIL" = 0 ] || exit 2
fi

if [ "$SPAWN_FAIL" != 0 ]; then
  exit 2
fi
