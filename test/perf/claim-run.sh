#!/usr/bin/env bash
# Measure an already-published release binary and retain reproducible artifacts.
# Usage: claim-run.sh BINARY OUTDIR
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BINARY="${1:-}"
OUTDIR="${2:-}"

if [ "$#" -ne 2 ]; then
  echo "usage: claim-run.sh BINARY OUTDIR" >&2
  exit 2
fi
case "$BINARY" in
/*) ;;
*) BINARY="$PWD/$BINARY" ;;
esac
[ -f "$BINARY" ] && [ -x "$BINARY" ] || {
  echo "claim-run: binary is not an executable file: $BINARY" >&2
  exit 2
}
mkdir -p "$OUTDIR"
OUTDIR="$(cd "$OUTDIR" && pwd -P)"
cd "$ROOT"

for tool in python3 git hyperfine script; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "claim-run: required command not found: $tool" >&2
    exit 1
  }
done

# Fail before a long run if a harness interface this script relies on has drifted.
for requirement in '--bin)' '--worktrees)' '--json)'; do
  grep -Fq "$requirement" "$HERE/cpu-sample.sh" || {
    echo "claim-run: cpu-sample.sh no longer accepts $requirement" >&2
    exit 1
  }
  if [ "$requirement" != '--worktrees)' ]; then
    grep -Fq "$requirement" "$HERE/flood.sh" || {
      echo "claim-run: flood.sh no longer accepts $requirement" >&2
      exit 1
    }
  fi
done
HYPERFINE_HELP="$(hyperfine --help 2>&1)"
grep -Fq -- '--runs' <<<"$HYPERFINE_HELP" || {
  echo "claim-run: hyperfine lacks --runs" >&2
  exit 1
}
grep -Fq -- '--export-json' <<<"$HYPERFINE_HELP" || {
  echo "claim-run: hyperfine lacks --export-json" >&2
  exit 1
}
grep -Fq -- '--command-timeout' <<<"$HYPERFINE_HELP" || {
  echo "claim-run: hyperfine lacks --command-timeout" >&2
  exit 1
}

# Each command has its own deadline. Kill the full process group on timeout so
# PTY wrappers cannot leave a measured process running in the background.
bounded() {
  local seconds="$1"
  shift
  python3 - "$seconds" "$@" <<'PY'
import os
import signal
import subprocess
import sys

seconds = int(sys.argv[1])
command = sys.argv[2:]
process = subprocess.Popen(command, start_new_session=True)
try:
    status = process.wait(timeout=seconds)
except subprocess.TimeoutExpired:
    print(f"claim-run: timed out after {seconds}s: {command[0]}", file=sys.stderr)
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    sys.exit(124)
sys.exit(status)
PY
}

SCRATCH="$(mktemp -d "/tmp/thegn-claim.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
STATE="$SCRATCH/state"
LAUNCH="$SCRATCH/launch.sh"
PREPARE="$SCRATCH/prepare.sh"
printf '#!/usr/bin/env bash\nset -euo pipefail\nexport XDG_STATE_HOME=%q\nexport THEGN_NO_MIGRATE=1 THEGN_BENCH_FIRST_FRAME_EXIT=1\nexec %q\n' "$STATE" "$BINARY" >"$LAUNCH"
printf '#!/usr/bin/env bash\nset -euo pipefail\nrm -rf %q\n' "$STATE" >"$PREPARE"
chmod +x "$LAUNCH" "$PREPARE"

# The pty helper expands to the platform-specific script(1) CLI used by just bench.
# Hyperfine invokes this command through its own shell.
# shellcheck source=test/lib/pty.sh disable=SC1091
source "$ROOT/test/lib/pty.sh"
PTY_LAUNCH="$(pty_cmd "$LAUNCH")"

VERSION="$SCRATCH/binary-version.txt"
bounded 15 "$BINARY" --version >"$VERSION"
VERSION_TEXT="$(cat "$VERSION")"
VERSION_TAG="$(printf '%s\n' "$VERSION_TEXT" | sed -nE 's/.*([0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?).*/v\1/p' | head -1)"
GIT_TAG="unknown"
TAG_COMMIT="unknown"
if [ -n "$VERSION_TAG" ] && git -C "$ROOT" rev-parse -q --verify "refs/tags/$VERSION_TAG^{commit}" >/dev/null; then
  GIT_TAG="$VERSION_TAG"
  TAG_COMMIT="$(git -C "$ROOT" rev-parse "$VERSION_TAG^{commit}")"
else
  GIT_TAG="$(git -C "$ROOT" describe --tags --exact-match HEAD 2>/dev/null || echo unknown)"
  if [ "$GIT_TAG" != unknown ]; then TAG_COMMIT="$(git -C "$ROOT" rev-parse "$GIT_TAG^{commit}")"; fi
fi
CHECKOUT_COMMIT="$(git -C "$ROOT" rev-parse HEAD)"

echo "claim-run: cold launch (30 measured runs)"
bounded 1800 hyperfine --runs 30 --warmup 3 --command-timeout 30 \
  --prepare "$PREPARE" --export-json "$OUTDIR/launch-cold.json" "$PTY_LAUNCH" \
  >"$OUTDIR/launch-cold.log" 2>&1

echo "claim-run: warm launch (30 measured runs)"
# The initial warm-up uses the same PTY command and persistent isolated state.
bounded 30 bash -c "$PTY_LAUNCH" >"$OUTDIR/launch-warm-prime.log" 2>&1
bounded 1800 hyperfine --runs 30 --warmup 3 --command-timeout 30 \
  --export-json "$OUTDIR/launch-warm.json" "$PTY_LAUNCH" \
  >"$OUTDIR/launch-warm.log" 2>&1

for n in 1 8 32; do
  echo "claim-run: idle CPU and RSS ($n worktrees)"
  bounded 180 "$HERE/cpu-sample.sh" --scenario idle --bin "$BINARY" \
    --worktrees "$n" --json >"$OUTDIR/idle-$n.json" 2>"$OUTDIR/idle-$n.log"
  echo "claim-run: host and daemon RSS ($n worktrees)"
  bounded 180 "$HERE/cpu-sample.sh" --scenario soak-daemon --bin "$BINARY" \
    --worktrees "$n" --json >"$OUTDIR/soak-daemon-$n.json" 2>"$OUTDIR/soak-daemon-$n.log"
done

echo "claim-run: flood switch and input latency"
bounded 360 "$HERE/flood.sh" --bin "$BINARY" --json \
  >"$OUTDIR/flood.json" 2>"$OUTDIR/flood.log"

OS_NAME="$(uname -s)"
ARCH="$(uname -m)"
case "$OS_NAME/$ARCH" in
Linux/x86_64)
  HOST_CLASS=linux-x86_64
  KERNEL="$(uname -srv)"
  ;;
Darwin/arm64 | Darwin/aarch64)
  HOST_CLASS=macos-arm64
  KERNEL="$(uname -srv)"
  ;;
*)
  OS_LOWER="$(printf '%s' "$OS_NAME" | tr '[:upper:]' '[:lower:]')"
  HOST_CLASS="$OS_LOWER-$ARCH"
  KERNEL="$(uname -srv)"
  ;;
esac
CPU_MODEL="unknown"
if [ "$OS_NAME" = Linux ] && [ -r /proc/cpuinfo ]; then
  CPU_MODEL="$(awk -F: '/model name/ { sub(/^[ \t]+/, "", $2); print $2; exit }' /proc/cpuinfo)"
elif [ "$OS_NAME" = Darwin ]; then
  CPU_MODEL="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || sysctl -n hw.model 2>/dev/null || echo unknown)"
fi
CPU_COUNT="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
TERMINAL="${TERM:-unknown}"
GEOMETRY="unknown"
if [ -t 0 ]; then GEOMETRY="$(stty size </dev/tty 2>/dev/null | awk '{print $2 "x" $1}')"; fi

THEGN_ROOT="$ROOT" THEGN_OUTDIR="$OUTDIR" THEGN_BINARY="$BINARY" \
  THEGN_VERSION_TEXT="$VERSION_TEXT" THEGN_GIT_TAG="$GIT_TAG" \
  THEGN_TAG_COMMIT="$TAG_COMMIT" THEGN_CHECKOUT_COMMIT="$CHECKOUT_COMMIT" \
  THEGN_HOST_CLASS="$HOST_CLASS" THEGN_KERNEL="$KERNEL" \
  THEGN_CPU_MODEL="${CPU_MODEL:-unknown}" THEGN_CPU_COUNT="$CPU_COUNT" \
  THEGN_TERMINAL="$TERMINAL" THEGN_GEOMETRY="$GEOMETRY" \
  THEGN_RUN_DATE="$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
  python3 - <<'PY'
import json
import os
from pathlib import Path

out = Path(os.environ["THEGN_OUTDIR"])
def load(name):
    with (out / name).open(encoding="utf-8") as stream:
        return json.load(stream)

idle = {n: load(f"idle-{n}.json") for n in (1, 8, 32)}
soak = {n: load(f"soak-daemon-{n}.json") for n in (1, 8, 32)}
flood = load("flood.json")
cold = load("launch-cold.json")["results"][0]
warm = load("launch-warm.json")["results"][0]
manifest = {
    "run_date_utc": os.environ["THEGN_RUN_DATE"],
    "binary": os.environ["THEGN_BINARY"],
    "binary_version": os.environ["THEGN_VERSION_TEXT"].strip(),
    "git_tag": os.environ["THEGN_GIT_TAG"],
    "tag_commit": os.environ["THEGN_TAG_COMMIT"],
    "checkout_commit": os.environ["THEGN_CHECKOUT_COMMIT"],
    "host_class": os.environ["THEGN_HOST_CLASS"],
    "kernel_or_os": os.environ["THEGN_KERNEL"],
    "cpu_model": os.environ["THEGN_CPU_MODEL"],
    "cpu_count": os.environ["THEGN_CPU_COUNT"],
    "terminal": os.environ["THEGN_TERMINAL"],
    "geometry": os.environ["THEGN_GEOMETRY"],
    "worktree_counts": [1, 8, 32],
    "cache_state": {"launch_cold": "fresh XDG_STATE_HOME before each run", "launch_warm": "reused XDG_STATE_HOME"},
    "launch_runs": 30,
    "launch_warmups": 3,
    "idle_measurements_per_worktree_count": 1,
    "idle": {str(n): {"worktrees": idle[n].get("worktrees"), "window_ms": idle[n].get("window_ms"), "cores_total": idle[n].get("cores_total")} for n in idle},
    "rss": {str(n): {"host_kb": soak[n].get("rss_host_kb"), "daemon_kb": soak[n].get("rss_daemon_kb"), "window_ms": soak[n].get("window_ms")} for n in soak},
    "flood": {"rollups": flood.get("rollups"), "switch_samples": flood.get("switch_samples_total"), "input_samples": flood.get("input_samples_total")},
    "percentile_note": "Histogram percentiles are lower bucket edges; fewer than 100 samples are insufficient for p99 claims.",
}
(out / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")

def percentile(value, count):
    if count < 100:
        return f"insufficient samples ({count}/100)"
    return f"{value} µs"

lines = [
    "# Release performance claim run",
    "",
    f"- Binary version: `{manifest['binary_version']}`",
    f"- Run date: `{manifest['run_date_utc']}`",
    f"- Git tag / commit: `{manifest['git_tag']}` / `{manifest['tag_commit']}`",
    f"- Checkout commit: `{manifest['checkout_commit']}`",
    f"- Host class: `{manifest['host_class']}`; OS/kernel: `{manifest['kernel_or_os']}`",
    f"- CPU: `{manifest['cpu_model']}` ({manifest['cpu_count']} logical cores)",
    f"- Terminal: `{manifest['terminal']}`, geometry `{manifest['geometry']}`",
    "- Launch: 30 runs per cache state, 3 warmups; cold clears isolated state before each run.",
    "- Flood percentiles show the worst rollup percentile; counts sum the samples across captured rollups.",
    "",
    "| Measurement | Result | Samples / setup |",
    "| --- | --- | --- |",
    f"| Cold launch → first frame | median {cold['median'] * 1000:.2f} ms | {len(cold['times'])} runs; raw values below |",
    f"| Warm launch → first frame | median {warm['median'] * 1000:.2f} ms | {len(warm['times'])} runs; raw values below |",
    "",
    "| Launch run | Cold (ms) | Warm (ms) |",
    "| ---: | ---: | ---: |",
]
for i, (cold_s, warm_s) in enumerate(zip(cold["times"], warm["times"]), start=1):
    lines.append(f"| {i} | {cold_s * 1000:.2f} | {warm_s * 1000:.2f} |")
lines.extend([
    "",
    "| Measurement | Result | Samples / setup |",
    "| --- | --- | --- |",
])
for n in (1, 8, 32):
    m = manifest["idle"][str(n)]
    rss = manifest["rss"][str(n)]
    lines.append(f"| Idle cores | {m['cores_total']} | {n} worktrees; {m['window_ms']} ms window |")
    lines.append(f"| Host / daemon RSS | {rss['host_kb']} KB / {rss['daemon_kb']} KB | soak-daemon, {n} worktrees; {rss['window_ms']} ms window |")
for metric, prefix, count_key in (("Switch → frame", "switch", "switch_samples_total"), ("Input → frame", "input", "input_samples_total")):
    count = int(flood.get(count_key, 0))
    vals = [percentile(flood.get(f"{prefix}_p{p}_us", 0), count) for p in (50, 95, 99)]
    lines.append(f"| {metric} p50 / p95 / p99 | {' / '.join(vals)} | {count} samples; {flood.get('rollups', 0)} rollups |")
lines.extend([
    f"| PTY throughput | {flood.get('pty_bytes_per_s', 'not reported')} bytes/s | {flood.get('rollups', 0)} rollups |",
    "",
    "Percentiles from runtime histograms are bucket lower edges, not exact elapsed times. See raw JSON and logs in this directory for every run.",
    "",
])
(out / "summary.md").write_text("\n".join(lines), encoding="utf-8")
PY

echo "claim-run: results written to $OUTDIR/summary.md"
