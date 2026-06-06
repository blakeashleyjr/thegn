#!/usr/bin/env bash
# test/coverage-drawer.sh — combined unit + integration line coverage for the
# bottom file-manager drawer, gated at 95% on the two fully-new modules
# (src/yazi.rs and src/commands/files.rs).
#
# Unit tests cover the pure logic; the in-session orchestration (run/spawn/
# close/restore) is only reachable through a real session, so we instrument the
# release binary and ALSO run the hermetic smoke test + the sandboxed pty drawer
# harness, merging every profile. cargo-llvm-cov's manual "show-env" mode lets
# the instrumented binary's subprocesses (spawned by the pty session) contribute.
#
# Needs the dev shell (cargo-llvm-cov, zellij, yazi, pyte). SKIPs the pty leg if
# zellij/yazi are unavailable — the smoke + unit legs still run.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

GATE="${COVERAGE_GATE:-95}"
GATED_FILES=("src/yazi.rs" "src/commands/files.rs")

export CARGO_TERM_COLOR=never
cargo llvm-cov clean --workspace

# Manual mode: export the instrumentation env so every `cargo`/binary run under
# it (and the binary's children) writes into the same profile set.
# shellcheck disable=SC1090
source <(cargo llvm-cov show-env --export-prefix)

echo "== build (instrumented) =="
cargo build --release
just build-plugins >/dev/null 2>&1 || cargo build --release --target wasm32-wasip1 \
  --manifest-path plugin/statusbar/Cargo.toml >/dev/null 2>&1 || true

echo "== unit tests =="
cargo test

echo "== smoke (hermetic) =="
./test/smoke.sh target/release/superzej || true

echo "== drawer pty harness (sandboxed) =="
if command -v zellij >/dev/null 2>&1; then
  python3 test/files-drawer.py || true
  SZ_TEST_DRAWER_WIDTH=center python3 test/files-drawer.py || true
else
  echo "  (skipped: zellij not on PATH)"
fi

echo "== merged report =="
cargo llvm-cov report --summary-only \
  --ignore-filename-regex '(plugin/|tests?/|/registry/)'

# Gate the fully-new modules.
json="$(mktemp)"
cargo llvm-cov report --json --summary-only >"$json"
fail=0
for f in "${GATED_FILES[@]}"; do
  pct="$(jq -r --arg f "$f" \
    '[.data[0].files[] | select(.filename | endswith($f)) | .summary.lines.percent] | first // 0' \
    "$json")"
  printf '  %-26s %6.2f%%  (gate %s%%)\n' "$f" "$pct" "$GATE"
  awk -v p="$pct" -v g="$GATE" 'BEGIN{exit !(p+0 >= g+0)}' || {
    echo "    BELOW GATE"
    fail=1
  }
done
rm -f "$json"

if [[ $fail -ne 0 ]]; then
  echo "coverage gate FAILED (need >= ${GATE}% on the new drawer modules)"
  exit 1
fi
echo "coverage gate PASSED (>= ${GATE}% on the new drawer modules)"
