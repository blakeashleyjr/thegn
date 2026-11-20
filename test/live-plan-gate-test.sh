#!/usr/bin/env bash
# Offline regression test for the three live scripts (b169 live-checks):
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
printf 'thegn-log %s\n' "${THEGN_LOG:-unset}" >>"$STUB_LOG"
if [ "${1:-}" = --set ]; then shift 2; fi
if [ "${THEGN_LOG:-}" = 'thegn::provider=trace' ]; then
  if [ -n "${THEGN_TRACE_FILE:-}" ]; then
    printf '%s\n' '2026-11-19T00:00:00Z TRACE sprites exec frame' >>"$THEGN_TRACE_FILE"
  fi
fi
if [ "${1:-}" = doctor ] && [ -n "${STUB_DOCTOR:-}" ]; then
  cat "$STUB_DOCTOR"
  exit 0
fi
if [ "${STUB_MODE:-}" = continue ]; then
  case "$*" in
  *thegn-memprobe*)
    case "${STUB_MEMPROBE:-}" in
    enforced) printf 'avail 2041\nreached 1984\n'; exit 137 ;;
    overshoot) printf 'avail 16000\nreached 2496\n'; exit 137 ;;
    *) printf 'avail 16000\nreached 2560\ndone 2560\n'; exit 0 ;;
    esac
    ;;
  *"command -v python3"*) printf '/usr/bin/python3\n/usr/bin/timeout\n'; exit 0 ;;
  *memory.max*) echo max; exit 0 ;;
  daemon) printf '%s\n' "$$" >"$STUB_LOG.daemon-pid"; exec tail -f /dev/null ;;
  "daemon stop") echo 'no thegn pane daemon is running'; exit 0 ;;
  "session list --json") echo '[]'; exit 0 ;;
  *"agent run"*)
    if [ "${STUB_AGENT_FAILURE:-}" = 1 ]; then
      printf '%s\n' '{"status":"failed","exit_code":7,"error":"agent boom","token":"SECRET","output_tail":"line one\nboom from agent"}'
      exit 3
    fi
    ;;
  "env test sx") exit 1 ;;
  "env set sx") exit 0 ;;
  "env up")
    mkdir -p "$XDG_STATE_HOME/thegn/vps"
    printf '%s\n' '{"provider":"sprites","state":"ready"}' >"$XDG_STATE_HOME/thegn/vps/sp-live.json"
    exit 0
    ;;
  "env pull") echo 'fast-forwarded sp-live'; exit 0 ;;
  "env down") rm -f "$XDG_STATE_HOME/thegn/vps/sp-live.json"; touch "$XDG_STATE_HOME/down"; exit 0 ;;
  "env down --worktree "*)
    rm -f "$XDG_STATE_HOME/thegn/vps/sp-live.json"
    touch "$XDG_STATE_HOME/down"
    touch "$STUB_LOG.fallback-down"
    exit 0
    ;;
  "env reap --provider sprites --unrecorded --json") echo '{"apply":false,"results":[],"accounts":[{"entries":[{"name":"sx-foreign","class":"unrecorded"}]}]}'; exit 0 ;;
  "env reap"*) echo '{"orphans":[]}'; exit 0 ;;
  "env deprovision"*)
    if [ "${STUB_DEPROVISION_REFUSED:-}" = 1 ]; then exit 1; fi
    exit 0
    ;;
  "fleet list"*)
    if [ -e "$XDG_STATE_HOME/down" ]; then
      if [ "${STUB_DEPROVISION_REFUSED:-}" = 1 ] &&
        [ -e "$STUB_LOG.deprov-list-seen" ] &&
        [ ! -e "$STUB_LOG.fallback-down" ]; then
        echo '{"view":{"rows":[{"id":"sp-live","state":"warm"}],"partial":false}}'
      else
        echo '{"view":{"rows":[],"down":[{"id":"sp-live"}],"missing":[],"partial":false}}'
        if [ "${STUB_DEPROVISION_REFUSED:-}" = 1 ] && [ ! -e "$STUB_LOG.deprov-list-seen" ]; then
          touch "$STUB_LOG.deprov-list-seen"
          mkdir -p "$XDG_STATE_HOME/thegn/vps"
          printf '%s\n' '{"provider":"sprites","state":"ready"}' >"$XDG_STATE_HOME/thegn/vps/sp-live.json"
        fi
      fi
    elif [ "${STUB_FLEET_OTHER_COLD:-}" = 1 ]; then
      echo '{"view":{"rows":[{"id":"sx-foreign","state":"cold"}],"partial":false}}'
    elif [ -e "$STUB_LOG.fleet-cost" ] && [ "${STUB_FLEET_PARTIAL_AFTER_COST:-}" = 1 ] && [ "${STUB_FLEET_PARTIAL:-}" = 1 ]; then
      echo '{"view":{"rows":[],"partial":true}}'
    elif [ -e "$STUB_LOG.fleet-cost" ] && [ "${STUB_FLEET_RUNNING_THEN_PARTIAL:-}" = 1 ]; then
      if [ -s "$STUB_LOG.fleet-running-reads" ]; then
        echo '{"view":{"rows":[],"partial":true}}'
      else
        echo 1 >"$STUB_LOG.fleet-running-reads"
        echo '{"view":{"rows":[{"id":"sp-live","state":"running"}],"partial":false}}'
      fi
    elif [ -e "$STUB_LOG.fleet-cost" ] && [ "${STUB_FLEET_RUNNING_READS:-0}" -gt 0 ]; then
      running_reads=0
      if [ -f "$STUB_LOG.fleet-running-reads" ]; then
        read -r running_reads <"$STUB_LOG.fleet-running-reads" || running_reads=0
      fi
      if [ "$running_reads" -lt "${STUB_FLEET_RUNNING_READS:-0}" ]; then
        printf '%s\n' "$((running_reads + 1))" >"$STUB_LOG.fleet-running-reads"
        echo '{"view":{"rows":[{"id":"sp-live","state":"running"}],"partial":false}}'
      else
        echo '{"view":{"rows":[{"id":"sp-live","state":"warm"}],"partial":false}}'
      fi
    elif [ "${STUB_FLEET_PARTIAL:-}" = 1 ] && [ "${STUB_FLEET_PARTIAL_AFTER_COST:-}" != 1 ]; then
      echo '{"view":{"rows":[],"partial":true}}'
    else
      echo '{"view":{"rows":[{"id":"sp-live","state":"warm"}],"partial":false}}'
    fi
    exit 0
    ;;
  "fleet cost"*)
    : >"$STUB_LOG.fleet-cost"
    : >"$STUB_LOG.fleet-running-reads"
    if [ "${STUB_COST_REPORTED_FALSE:-}" = 1 ]; then
      echo '{"sprites_usage":[{"env":"sx","error":null,"rows":[{"name":"sp-live","cpu_hours":0.0,"partial":false,"reported":false}]}]}'
    else
      echo '{"sprites_usage":[{"env":"sx","error":null,"rows":[{"name":"sp-live","cpu_hours":0.0,"partial":false}]}]}'
    fi
    exit 0
    ;;
  "fleet inspect"*) echo '{"memory_limit_mb":2048}'; exit 0 ;;
  *"exit 3"*)
    if [ "${STUB_EXEC_STDERR:-}" = 1 ]; then echo boom >&2; else echo boom; fi
    exit 3
    ;;
  "sprite-exec sp-live -- true") exit 0 ;;
  *"--hold"*) /usr/bin/sleep 2; exit 0 ;;
  *"git -c user.name=live"*) echo '1111111111111111111111111111111111111111'; exit 0 ;;
  *"rev-parse HEAD"*) echo '1111111111111111111111111111111111111111'; exit 0 ;;
  *"command -v curl"*) echo /usr/bin/curl; exit 0 ;;
  *"tty && echo ok"*) printf '/dev/pts/1\nok\n'; exit 0 ;;
  *"curl"*"/v1/tasks"*) echo '[]'; exit 0 ;;
  *"curl"*"/v1/usage"*) printf '200'; exit 0 ;;
  esac
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
cat >"$T/bin/sleep" <<'STUB'
#!/usr/bin/env bash
exit 0
STUB
chmod +x "$T/bin/thegn" "$T/bin/curl" "$T/bin/sleep"

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
  rm -f "$LOG.fleet-cost" "$LOG.fleet-running-reads"
  env -u THEGN_BIN -u THEGN_CHANNEL -u STUB_DOCTOR -u THEGN_LOG PATH="$T/bin:$PATH" STUB_LOG="$LOG" TMPDIR="$T/tmp" \
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
    has "$T/plan-$s.out" "0   preflight" &&
    has "$T/plan-$s.out" "STARTUP lines (start-up numbers)"; then
    pass "$s: plan names the preflight"
  else
    fail "$s: plan lacks the preflight statement"
  fi
done
run plan-sprites-keep-going sprites-live.sh -- --keep-going
if rc_is plan-sprites-keep-going 0 && [ ! -s "$T/plan-sprites-keep-going.log" ] &&
  has "$T/plan-sprites-keep-going.out" "FAILED STEPS:"; then
  pass "sprites-live: --keep-going is a plan-only flag"
else
  fail "sprites-live: --keep-going plan ($(cat "$T/plan-sprites-keep-going.out"))"
fi
run plan-sprites-with-agent sprites-live.sh -- --with-agent gate_agent
if rc_is plan-sprites-with-agent 0 && [ ! -s "$T/plan-sprites-with-agent.log" ] &&
  has "$T/plan-sprites-with-agent.out" "spends model usage"; then
  pass "sprites-live: --with-agent without spend flags is plan-only"
else
  fail "sprites-live: --with-agent plan ($(cat "$T/plan-sprites-with-agent.out"))"
fi
for want in "--keep-going" "view.rows" "4b " "10w " "--with-agent" "6c " "THE-2450" "8c " "cpu_hours" "zero usage (partial or not)" "FAILED STEPS:"; do
  if has "$T/plan-sprites-live.out" "$want"; then
    pass "sprites-live plan names '$want'"
  else
    fail "sprites-live plan lacks '$want'"
  fi
done
for want in "11b " "view.down" "view.missing" "binary predates batch 170" "13 " "apply == false" "unrecorded count"; do
  if has "$T/plan-sprites-live.out" "$want"; then
    pass "sprites-live plan names '$want'"
  else
    fail "sprites-live plan lacks '$want'"
  fi
done
for want in "5b " "5d " "6b " "7e " "8d " "8f " "10b " "10u (INFO) direct usage reads"; do
  if has "$T/plan-sprites-live.out" "$want"; then pass "sprites-live plan names step $want"; else fail "sprites-live plan lacks step $want"; fi
done
if has "$T/plan-sprites-live.out" "fails only if it stays running"; then
  pass "sprites-live plan explains the post-cost wake gate"
else
  fail "sprites-live plan lacks the post-cost wake gate explanation"
fi
if has "$T/plan-sprites-live.out" "never secrets"; then
  pass "sprites-live plan explains sanitized agent failures"
else
  fail "sprites-live plan lacks sanitized agent failure text"
fi
for want in " 11 " "11b " "11c " "wt rm <rd-y path> --force"; do
  if has "$T/plan-remote-daily.out" "$want"; then pass "remote-daily plan names '$want'"; else fail "remote-daily plan lacks '$want'"; fi
done
for want in "11k " "11m " "11j "; do
  if has "$T/plan-remote-daily.out" "$want"; then pass "remote-daily plan names '$want'"; else fail "remote-daily plan lacks '$want'"; fi
done
for want in "released sandbox <name>" "pulled tg/rd-y"; do
  if has "$T/plan-remote-daily.out" "$want"; then pass "remote-daily plan names '$want'"; else fail "remote-daily plan lacks '$want'"; fi
done
if has "$T/plan-remote-daily.out" 'gate_command="true"'; then
  pass "remote-daily lands override the scratch repository gate command"
else
  fail "remote-daily plan lacks the scratch repository gate override"
fi
for want in "3s " "7s "; do
  if has "$T/plan-vercel-live.out" "$want"; then pass "vercel-live plan names '$want'"; else fail "vercel-live plan lacks '$want'"; fi
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
if has "$T/pf-fly.out" "TIME 1 "; then
  pass "preflight: remote-daily records step timing"
else
  fail "preflight: remote-daily missing step timing"
fi

run pf-vx vercel-live.sh THEGN_CHANNEL=dev STUB_DOCTOR="$T/vx.json" -- \
  --token-ref env:VX_NAME_ONLY --team-id team_x --project-id prj_x --i-accept-spend
if has "$T/pf-vx.out" "INFO 0: credentials entering the sandbox: forwarded_env=none; git_token=no; login_upload=none" &&
  has "$T/pf-vx.out" "PASS 0" && has "$T/pf-vx.log" "env test vxlive" && ! has "$T/pf-vx.out" "WARN 0"; then
  pass "preflight: vercel-live reads the temp env's row"
else
  fail "preflight: vercel-live ($(cat "$T/pf-vx.out"))"
fi
if has "$T/pf-vx.out" "TIME 2 "; then
  pass "preflight: vercel-live records step timing"
else
  fail "preflight: vercel-live missing step timing"
fi

run keep-going-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if ! rc_is keep-going-sprites 0 && has "$T/keep-going-sprites.out" "FAIL 1b:" &&
  has "$T/keep-going-sprites.out" "PASS 4b" &&
  has "$T/keep-going-sprites.out" "arrived on stdout" &&
  has "$T/keep-going-sprites.out" "TIME 4b " &&
  has "$T/keep-going-sprites.out" "STARTUP cold create (env up): " &&
  has "$T/keep-going-sprites.out" "STARTUP wake from cold (sprite-exec -- true): " &&
  has "$T/keep-going-sprites.out" "== step 13:" &&
  has "$T/keep-going-sprites.out" "trace log path:" &&
  has "$T/keep-going-sprites.out" "trace lines: 1" &&
  has "$T/keep-going-sprites.out" "last 40 sprites exec frame lines:" &&
  has "$T/keep-going-sprites.out" "TRACE sprites exec frame" &&
  has "$T/keep-going-sprites.out" "WARN 10: usage not reported yet for a sprite this young" &&
  has "$T/keep-going-sprites.out" "INFO 10u: no usage read changed the reported state" &&
  has "$T/keep-going-sprites.out" "PASS 10u" &&
  has "$T/keep-going-sprites.out" "PASS 10w" &&
  has "$T/keep-going-sprites.out" "SKIP 5, 5b, 5c, 5d" &&
  has "$T/keep-going-sprites.out" "unrecorded entries: 1" &&
  has "$T/keep-going-sprites.log" "thegn-log thegn::provider=trace" &&
  has "$T/keep-going-sprites.out" "FAILED STEPS:" &&
  ! has "$T/keep-going-sprites.log" "env down --force"; then
  pass "sprites-live: --keep-going records a failure, continues, and avoids repeat force-down"
else
  fail "sprites-live: --keep-going acting run ($(cat "$T/keep-going-sprites.out"))"
fi

run exec-stderr-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_EXEC_STDERR=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if has "$T/exec-stderr-sprites.out" "PASS 4b" &&
  has "$T/exec-stderr-sprites.out" "arrived on stderr"; then
  pass "sprites-live: sprite-exec stderr marker is accepted on stderr"
else
  fail "sprites-live: sprite-exec stderr marker ($(cat "$T/exec-stderr-sprites.out"))"
fi

run memprobe-enforced sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_MEMPROBE=enforced STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --min-ram-gb 2 --keep-going
if has "$T/memprobe-enforced.out" "PASS 5d" &&
  has "$T/memprobe-enforced.out" "memory limit enforced" &&
  has "$T/memprobe-enforced.out" "INFO 5d: guest cgroup memory.max: max" &&
  has "$T/memprobe-enforced.out" "INFO 5d: guest MemAvailable: 2041 MiB (policy 2048 MiB)"; then
  pass "sprites-live: memory probe reports enforcement"
else
  fail "sprites-live: enforced memory probe ($(cat "$T/memprobe-enforced.out"))"
fi

run memprobe-unenforced sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_MEMPROBE=unenforced STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --min-ram-gb 2 --keep-going
if has "$T/memprobe-unenforced.out" "WARN 5d: limit accepted by the API but not enforced in the guest" &&
  has "$T/memprobe-unenforced.out" "PASS 5d" &&
  has "$T/memprobe-unenforced.out" "INFO 5d: guest cgroup memory.max: max" &&
  has "$T/memprobe-unenforced.out" "INFO 5d: guest MemAvailable: 16000 MiB (policy 2048 MiB)"; then
  pass "sprites-live: memory probe warns when unenforced"
else
  fail "sprites-live: unenforced memory probe ($(cat "$T/memprobe-unenforced.out"))"
fi

run memprobe-overshoot sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_MEMPROBE=overshoot STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --min-ram-gb 2 --keep-going
if has "$T/memprobe-overshoot.out" "WARN 5d: the probe was stopped at 2496 MiB, above the 2048 MiB limit" &&
  has "$T/memprobe-overshoot.out" "PASS 5d"; then
  pass "sprites-live: memory probe warns on overshoot"
else
  fail "sprites-live: overshoot memory probe ($(cat "$T/memprobe-overshoot.out"))"
fi

run reported-false-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_COST_REPORTED_FALSE=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if ! rc_is reported-false-sprites 0 &&
  has "$T/reported-false-sprites.out" "WARN 10: no usage reported yet (the Sprites usage API has no record for this sprite yet)"; then
  pass "sprites-live: fleet cost warns when usage is not reported"
else
  fail "sprites-live: reported false usage ($(cat "$T/reported-false-sprites.out"))"
fi

run other-cold-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_FLEET_OTHER_COLD=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if ! rc_is other-cold-sprites 0 && has "$T/other-cold-sprites.out" "FAIL 9:"; then
  pass "sprites-live: row-specific warm/cold check rejects a foreign cold sprite"
else
  fail "sprites-live: foreign cold row ($(cat "$T/other-cold-sprites.out"))"
fi

run cost-running-once-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_FLEET_RUNNING_READS=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if has "$T/cost-running-once-sprites.out" "WARN 10b: the sprite read running 1 time(s)" &&
  has "$T/cost-running-once-sprites.out" "PASS 10b"; then
  pass "sprites-live: one post-cost running read settles with a warning"
else
  fail "sprites-live: one post-cost running read ($(cat "$T/cost-running-once-sprites.out"))"
fi

run cost-running-stuck-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_FLEET_RUNNING_READS=9 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if has "$T/cost-running-stuck-sprites.out" "FAIL 10b" &&
  has "$T/cost-running-stuck-sprites.out" "stayed running"; then
  pass "sprites-live: four post-cost running reads fail the wake gate"
else
  fail "sprites-live: stuck post-cost running row ($(cat "$T/cost-running-stuck-sprites.out"))"
fi

run cost-list-partial-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_FLEET_PARTIAL=1 STUB_FLEET_PARTIAL_AFTER_COST=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if ! has "$T/cost-list-partial-sprites.out" "FAIL 10:" &&
  has "$T/cost-list-partial-sprites.out" "WARN 10b: the provider listing could not be read after fleet cost; the wake check is unproven" &&
  has "$T/cost-list-partial-sprites.out" "PASS 10b"; then
  pass "sprites-live: an unreadable post-cost listing warns and passes"
else
  fail "sprites-live: partial post-cost listing ($(cat "$T/cost-list-partial-sprites.out"))"
fi

run cost-running-partial-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_FLEET_RUNNING_THEN_PARTIAL=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if has "$T/cost-running-partial-sprites.out" "FAIL 10b" &&
  has "$T/cost-running-partial-sprites.out" "stayed running" &&
  ! has "$T/cost-running-partial-sprites.out" "wake check is unproven"; then
  pass "sprites-live: a running read followed by partial listings fails the wake gate"
else
  fail "sprites-live: running then partial post-cost listing ($(cat "$T/cost-running-partial-sprites.out"))"
fi

run deprovision-refused-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_DEPROVISION_REFUSED=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going
if has "$T/deprovision-refused-sprites.log" "thegn env down --worktree" &&
  has "$T/deprovision-refused-sprites.out" "FAILED STEPS: 1b" &&
  ! has "$T/deprovision-refused-sprites.out" "FAILED STEPS: 1b, 11"; then
  pass "sprites-live: refused deprovision falls back to worktree down without changing the result"
else
  fail "sprites-live: refused deprovision fallback ($(cat "$T/deprovision-refused-sprites.out"))"
fi

rm -f "$LOG.daemon-pid"
run agent-daemon-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going --with-agent stubagent
daemon_pid="$(cat "$LOG.daemon-pid" 2>/dev/null)" || daemon_pid=""
if [ -n "$daemon_pid" ] &&
  ! kill -0 "$daemon_pid" 2>/dev/null &&
  grep -Fxq "thegn daemon" "$T/agent-daemon-sprites.log" &&
  grep -Fxq "thegn session list --json" "$T/agent-daemon-sprites.log" &&
  grep -Fxq "thegn daemon stop" "$T/agent-daemon-sprites.log" &&
  has "$T/agent-daemon-sprites.out" "pane daemon up (pid $daemon_pid)" &&
  has "$T/agent-daemon-sprites.out" "-- teardown: daemon stop (this run's pane daemon" &&
  has "$T/agent-daemon-sprites.out" "== step 7g:"; then
  pass "sprites-live: --with-agent starts and stops its own pane daemon"
else
  if [ -n "$daemon_pid" ]; then kill "$daemon_pid" 2>/dev/null || true; fi
  fail "sprites-live: --with-agent pane daemon ($(cat "$T/agent-daemon-sprites.out"))"
fi

rm -f "$LOG.daemon-pid"
run agent-failure-sprites sprites-live.sh THEGN_CHANNEL=dev STUB_MODE=continue STUB_AGENT_FAILURE=1 STUB_DOCTOR="$T/tok-true.json" -- \
  "${SX[@]}" --keep-going --with-agent stubagent
if has "$T/agent-failure-sprites.out" "agent boom" &&
  has "$T/agent-failure-sprites.out" "boom from agent" &&
  has "$T/agent-failure-sprites.out" "exit_code" &&
  ! has "$T/agent-failure-sprites.out" "SECRET"; then
  pass "sprites-live: agent failure summary is bounded and sanitized"
else
  fail "sprites-live: agent failure summary ($(cat "$T/agent-failure-sprites.out"))"
fi
if ! grep -Fq "thegn daemon" "$T/keep-going-sprites.log"; then
  pass "sprites-live: no pane daemon without --with-agent"
else
  fail "sprites-live: a pane daemon was started without --with-agent"
fi

# Every acting run must clean up its temp state (nothing was created).
if [ -z "$(ls -A "$T/tmp")" ]; then
  pass "acting runs removed their temp state"
else
  fail "temp state left behind: $(ls "$T/tmp")"
fi

# ---- 3. shared live library ----
listing_present="$(PATH="$T/bin:$PATH" STUB_LOG="$LOG" STUB_MODE=continue XDG_STATE_HOME="$T/libstate" THEGN_BIN=thegn bash -c '
# shellcheck disable=SC2016
. "$1"
live_listing sx sp-live
' bash "$ROOT/test/lib/live.sh")"
if [ "$listing_present" = "present warm" ]; then
  pass "live library: present row reports its state"
else
  fail "live library: present row ($listing_present)"
fi
listing_absent="$(PATH="$T/bin:$PATH" STUB_LOG="$LOG" STUB_MODE=continue STUB_FLEET_OTHER_COLD=1 XDG_STATE_HOME="$T/libstate" THEGN_BIN=thegn bash -c '
# shellcheck disable=SC2016
. "$1"
live_listing sx sp-live
' bash "$ROOT/test/lib/live.sh")"
if [ "$listing_absent" = "absent" ]; then
  pass "live library: complete listing proves absence"
else
  fail "live library: absent row ($listing_absent)"
fi
listing_partial="$(PATH="$T/bin:$PATH" STUB_LOG="$LOG" STUB_MODE=continue STUB_FLEET_PARTIAL=1 XDG_STATE_HOME="$T/libstate" THEGN_BIN=thegn bash -c '
# shellcheck disable=SC2016
. "$1"
live_listing sx sp-live
' bash "$ROOT/test/lib/live.sh")"
if [ "$listing_partial" = "unknown" ]; then
  pass "live library: partial listing is unknown"
else
  fail "live library: partial listing ($listing_partial)"
fi
live_keep_output="$T/live-keep.out"
live_keep_rc=0
bash -c '
# shellcheck disable=SC2016
. "$1"
FAILED_STEPS=()
LIVE_KEEP_GOING=1
LIVE_STARTUP=("x=demo" "z=never")
live_step x ok true
live_step y no false
live_finish
' bash "$ROOT/test/lib/live.sh" >"$live_keep_output" 2>&1 || live_keep_rc=$?
if [ "$live_keep_rc" -eq 1 ] &&
  has "$live_keep_output" "PASS x" &&
  has "$live_keep_output" "TIME x " &&
  has "$live_keep_output" "FAIL y: no" &&
  has "$live_keep_output" "STARTUP demo: " &&
  has "$live_keep_output" "STARTUP never: not measured (step z did not run)" &&
  has "$live_keep_output" "FAILED STEPS: y"; then
  pass "live library: keep-going records failures and startup times"
else
  fail "live library: keep-going ($(cat "$live_keep_output"))"
fi
live_stop_output="$T/live-stop.out"
live_stop_rc=0
bash -c '
# shellcheck disable=SC2016
. "$1"
FAILED_STEPS=()
LIVE_KEEP_GOING=0
LIVE_STARTUP=("x=demo" "z=never")
live_step x ok true
live_step y no false
live_finish
' bash "$ROOT/test/lib/live.sh" >"$live_stop_output" 2>&1 || live_stop_rc=$?
if [ "$live_stop_rc" -eq 1 ] &&
  has "$live_stop_output" "FAIL y: no" &&
  ! has "$live_stop_output" "STARTUP"; then
  pass "live library: default failure stops before summary"
else
  fail "live library: stop-on-failure ($(cat "$live_stop_output"))"
fi

if [ "$fails" -gt 0 ]; then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "ALL PASS"
