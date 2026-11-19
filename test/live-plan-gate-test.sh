#!/usr/bin/env bash
# Offline regression test for the three live scripts (b166 live-checks):
# test/sprites-live.sh, test/remote-daily.sh and test/vercel-live.sh.
#
# - With no flags each prints its plan, exits 0 and runs neither thegn nor curl
#   (both are stubbed on PATH to log and fail loudly).
# - Step 0 (preflight) parses a stubbed `thegn doctor --json`: FAIL when the
#   token does not resolve or the env has no row, INFO credential line, WARN for
#   unbound sandboxes, SKIP when the remote_live key is absent (or fly).
#   The stub fails every other verb, so no run gets past `env test`.
#
# Usage: bash test/live-plan-gate-test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/tmp"
LOG="$T/calls.log"
: >"$LOG"

cat >"$T/bin/thegn" <<'STUB'
#!/usr/bin/env bash
printf 'thegn %s\n' "$*" >>"$STUB_LOG"
if [ "${1:-}" = doctor ] && [ -n "${STUB_DOCTOR:-}" ]; then
  cat "$STUB_DOCTOR"
  exit 0
fi
echo "stub thegn: refusing '$*'" >&2
exit 97
STUB
cat >"$T/bin/curl" <<'STUB'
#!/usr/bin/env bash
printf 'curl %s\n' "$*" >>"$STUB_LOG"
echo "stub curl: refusing" >&2
exit 97
STUB
chmod +x "$T/bin/thegn" "$T/bin/curl"

fails=0
pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1" >&2
  fails=$((fails + 1))
}
has() { grep -Fq -- "$2" "$1"; }

# run <name> <script> [env...] -- [args...]: output in $T/<name>.out, rc in $T/<name>.rc
run() {
  local name="$1" script="$2" rc=0
  shift 2
  local envs=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do
    envs+=("$1")
    shift
  done
  [ $# -gt 0 ] && shift
  : >"$LOG"
  env -u THEGN_BIN -u THEGN_CHANNEL -u STUB_DOCTOR PATH="$T/bin:$PATH" STUB_LOG="$LOG" TMPDIR="$T/tmp" \
    "${envs[@]}" bash "$ROOT/test/$script" "$@" >"$T/$name.out" 2>&1 </dev/null || rc=$?
  echo "$rc" >"$T/$name.rc"
  cp "$LOG" "$T/$name.log"
}
rc_is() { [ "$(cat "$T/$1.rc")" = "$2" ]; }

# ---- 1. no flags: plan only, exit 0, nothing called ----
for s in sprites-live remote-daily vercel-live; do
  run "plan-$s" "$s.sh" --
  if rc_is "plan-$s" 0 && [ ! -s "$T/plan-$s.log" ]; then
    pass "$s: no flags exits 0 without running thegn or curl"
  else
    fail "$s: no flags (rc $(cat "$T/plan-$s.rc"); calls: $(tr '\n' ';' <"$T/plan-$s.log"))"
  fi
  if has "$T/plan-$s.out" "The preflight prints which credentials enter the sandbox." &&
    has "$T/plan-$s.out" "0   preflight"; then
    pass "$s: plan names the preflight"
  else
    fail "$s: plan lacks the preflight statement"
  fi
done
for want in "5b " "6b " "7e " "10b "; do
  if has "$T/plan-sprites-live.out" "$want"; then pass "sprites-live plan names step $want"; else fail "sprites-live plan lacks step $want"; fi
done
for want in " 11 " "11b " "11c " "wt rm <rd-y path> --force"; do
  if has "$T/plan-remote-daily.out" "$want"; then pass "remote-daily plan names '$want'"; else fail "remote-daily plan lacks '$want'"; fi
done
if has "$T/plan-vercel-live.out" "9b  no ledger record"; then pass "vercel-live plan names step 9b"; else fail "vercel-live plan lacks step 9b"; fi

# ---- 2. preflight against stubbed doctor output ----
doctor() { # doctor <file> <json>
  printf '%s\n' "$2" >"$T/$1.json"
}
doctor tok-false '{"remote_live":[{"env":"sx","token_resolves":false,"forwarded_env":[],"git_token":false,"login_upload":"none","unbound":[]}]}'
doctor tok-true '{"checks":[],"remote_live":[{"env":"other","token_resolves":false},{"env":"sx","token_resolves":true,"forwarded_env":["GH_NAME_ONLY","NPM_NAME_ONLY"],"git_token":true,"login_upload":"claude","unbound":["sx-old-1","sx-old-2"]}]}'
doctor no-key '{"checks":[]}'
doctor no-row '{"remote_live":[{"env":"other","token_resolves":true}]}'
doctor vx '{"remote_live":[{"env":"vxlive","token_resolves":true,"forwarded_env":[],"git_token":false,"login_upload":"none","unbound":[]}]}'

SX=(--env sx --i-accept-spend)

run pf-false sprites-live.sh THEGN_CHANNEL=dev STUB_DOCTOR="$T/tok-false.json" -- "${SX[@]}"
if ! rc_is pf-false 0 && has "$T/pf-false.out" "FAIL 0: the token for env sx does not resolve" &&
  ! has "$T/pf-false.log" "env test"; then
  pass "preflight: unresolved token fails step 0 before env test"
else
  fail "preflight: unresolved token ($(cat "$T/pf-false.out"))"
fi

run pf-true sprites-live.sh THEGN_CHANNEL=dev STUB_DOCTOR="$T/tok-true.json" -- "${SX[@]}"
if has "$T/pf-true.out" "INFO 0: credentials entering the sandbox: forwarded_env=GH_NAME_ONLY,NPM_NAME_ONLY; git_token=yes; login_upload=claude" &&
  has "$T/pf-true.out" "WARN 0: unbound ledger sandboxes: sx-old-1, sx-old-2: pre-existing sandboxes this run will not touch" &&
  has "$T/pf-true.out" "PASS 0" && has "$T/pf-true.log" "env test sx" && ! rc_is pf-true 0; then
  pass "preflight: INFO credential line, WARN unbound, continues to env test"
else
  fail "preflight: resolved token ($(cat "$T/pf-true.out"))"
fi
if ! has "$T/pf-true.log" "curl" && ! has "$T/pf-true.log" "env set"; then
  pass "preflight: nothing is created when env test fails"
else
  fail "preflight: calls after a failed env test: $(tr '\n' ';' <"$T/pf-true.log")"
fi

run pf-nokey sprites-live.sh THEGN_CHANNEL=dev STUB_DOCTOR="$T/no-key.json" -- "${SX[@]}"
if has "$T/pf-nokey.out" "SKIP 0: doctor --json has no remote_live key" && has "$T/pf-nokey.log" "env test sx"; then
  pass "preflight: absent remote_live key skips with a note"
else
  fail "preflight: absent key ($(cat "$T/pf-nokey.out"))"
fi

run pf-norow remote-daily.sh THEGN_CHANNEL=dev STUB_DOCTOR="$T/no-row.json" -- "${SX[@]}"
if has "$T/pf-norow.out" "FAIL 0: doctor remote_live has no row for env sx" && ! has "$T/pf-norow.log" "env test"; then
  pass "preflight: remote-daily fails when the env has no row"
else
  fail "preflight: remote-daily no row ($(cat "$T/pf-norow.out"))"
fi

run pf-fly remote-daily.sh THEGN_CHANNEL=dev RD_HOLD_AGENT=1 STUB_DOCTOR="$T/no-row.json" -- "${SX[@]}" --provider fly
if has "$T/pf-fly.out" "SKIP 0: doctor remote_live has no row for env sx" && has "$T/pf-fly.log" "env test sx"; then
  pass "preflight: remote-daily skips the missing row on fly"
else
  fail "preflight: remote-daily fly ($(cat "$T/pf-fly.out"))"
fi

run pf-vx vercel-live.sh THEGN_CHANNEL=dev STUB_DOCTOR="$T/vx.json" -- \
  --token-ref env:VX_NAME_ONLY --team-id team_x --project-id prj_x --i-accept-spend
if has "$T/pf-vx.out" "INFO 0: credentials entering the sandbox: forwarded_env=none; git_token=no; login_upload=none" &&
  has "$T/pf-vx.out" "PASS 0" && has "$T/pf-vx.log" "env test vxlive" && ! has "$T/pf-vx.out" "WARN 0"; then
  pass "preflight: vercel-live reads the temp env's row"
else
  fail "preflight: vercel-live ($(cat "$T/pf-vx.out"))"
fi

# Every acting run must clean up its temp state (nothing was created).
if [ -z "$(ls -A "$T/tmp")" ]; then
  pass "acting runs removed their temp state"
else
  fail "temp state left behind: $(ls "$T/tmp")"
fi

if [ "$fails" -gt 0 ]; then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "ALL PASS"
