# shellcheck shell=bash
# Contract with the sourcing script:
# - THEGN_BIN is the thegn executable.
# - FAILED_STEPS=() is declared by the sourcing script.
# - LIVE_KEEP_GOING is 0 or 1, defaulting to 0.
# - LIVE_THEGN_ARGS is an array of leading thegn arguments, defaulting empty.
# - live_on_pass <id> is optional.
# - LIVE_STARTUP is an optional array of "<id>=<label>" entries.

LIVE_KEEP_GOING="${LIVE_KEEP_GOING:-0}"
LIVE_THEGN_ARGS=("${LIVE_THEGN_ARGS[@]+"${LIVE_THEGN_ARGS[@]}"}")
LIVE_TIME_IDS=()
LIVE_TIME_MS=()
LIVE_DOCTOR_JSON=""

live_now_ms() {
  local us s
  us="${EPOCHREALTIME:-}"
  us="${us//[!0-9]/}"
  if [ -n "$us" ]; then
    printf '%s\n' "$((us / 1000))"
    return 0
  fi
  s="$(date +%s)" || s=0
  printf '%s\n' "$((s * 1000))"
}

live_fmt_ms() {
  local ms="$1"
  printf '%s.%s s\n' "$((ms / 1000))" "$(((ms % 1000) / 100))"
}

live_record_time() {
  local id="$1" started_ms="$2" elapsed now
  now="$(live_now_ms)"
  elapsed=$((now - started_ms))
  [ "$elapsed" -ge 0 ] || elapsed=0
  LIVE_TIME_IDS+=("$id")
  LIVE_TIME_MS+=("$elapsed")
  printf 'TIME %s %s\n' "$id" "$(live_fmt_ms "$elapsed")"
}

live_time_of() {
  local id="$1" i found=1 result
  for i in "${!LIVE_TIME_IDS[@]}"; do
    if [ "${LIVE_TIME_IDS[$i]}" = "$id" ]; then
      result="${LIVE_TIME_MS[$i]}"
      found=0
    fi
  done
  [ "$found" -eq 0 ] || return 1
  printf '%s\n' "$result"
}

live_step() {
  local id="$1" desc="$2" started
  shift 2
  echo "== step $id: $desc"
  started="$(live_now_ms)"
  if "$@"; then
    echo "PASS $id"
    live_record_time "$id" "$started"
    if declare -F live_on_pass >/dev/null; then live_on_pass "$id"; fi
  else
    echo "FAIL $id: $desc" >&2
    live_record_time "$id" "$started"
    FAILED_STEPS+=("$id")
    if [ "$LIVE_KEEP_GOING" != 1 ]; then
      exit 1
    fi
  fi
  return 0
}

live_finish() {
  local entry id label elapsed
  for entry in "${LIVE_STARTUP[@]+"${LIVE_STARTUP[@]}"}"; do
    id="${entry%%=*}"
    label="${entry#*=}"
    if elapsed="$(live_time_of "$id")"; then
      printf 'STARTUP %s: %s (step %s)\n' "$label" "$(live_fmt_ms "$elapsed")" "$id"
    else
      printf 'STARTUP %s: not measured (step %s did not run)\n' "$label" "$id"
    fi
  done
  if [ "${#FAILED_STEPS[@]}" -gt 0 ]; then
    echo "FAILED STEPS: ${FAILED_STEPS[*]}"
    exit 1
  fi
  echo "ALL STEPS PASS"
}

LIVE_PREFLIGHT_PY='
import json, os, sys
env = os.environ["PRE_ENV"]
required = os.environ.get("PRE_REQUIRED", "1") == "1"
try:
    doc = json.loads(sys.stdin.read())
except ValueError:
    print("FAIL 0: doctor --json did not print JSON", file=sys.stderr)
    sys.exit(1)
rows = doc.get("remote_live") if isinstance(doc, dict) else None
if rows is None:
    print("SKIP 0: doctor --json has no remote_live key (older binary): no credential statement")
    sys.exit(0)
row = next((r for r in rows if isinstance(r, dict) and r.get("env") == env), None)
if row is None:
    if required:
        print("FAIL 0: doctor remote_live has no row for env " + env, file=sys.stderr)
        sys.exit(1)
    print("SKIP 0: doctor remote_live has no row for env " + env + " (it covers sprites and vercel envs)")
    sys.exit(0)
def show(v):
    if isinstance(v, bool):
        return "yes" if v else "no"
    if isinstance(v, list):
        return ",".join(show(x) for x in v) or "none"
    if isinstance(v, dict):
        return ",".join(k + "=" + show(x) for k, x in sorted(v.items())) or "none"
    if v is None or v == "":
        return "none"
    return str(v)
if row.get("token_resolves") is not True:
    print("FAIL 0: the token for env " + env + " does not resolve (remote_live.token_resolves = " + show(row.get("token_resolves")) + ")", file=sys.stderr)
    sys.exit(1)
print("INFO 0: credentials entering the sandbox: forwarded_env=" + show(row.get("forwarded_env")) + "; git_token=" + show(row.get("git_token")) + "; login_upload=" + show(row.get("login_upload")))
if os.environ.get("PRE_STATE_DIR"):
    with open(os.path.join(os.environ["PRE_STATE_DIR"], "preflight.env"), "w", encoding="utf-8") as f:
        f.write("LOGIN_UPLOAD=" + show(row.get("login_upload")) + "\n")
unbound = row.get("unbound") or []
if not isinstance(unbound, list):
    unbound = [unbound]
if unbound:
    print("WARN 0: unbound ledger sandboxes: " + ", ".join(show(n) for n in unbound) + ": pre-existing sandboxes this run will not touch")
print("PASS 0")
'

live_preflight_capture() {
  LIVE_DOCTOR_JSON="$("$THEGN_BIN" "${LIVE_THEGN_ARGS[@]+"${LIVE_THEGN_ARGS[@]}"}" doctor --json 2>/dev/null)" || true
}

live_preflight_check() {
  echo "== step 0: preflight (doctor --json remote_live for env '$1')"
  printf '%s' "$LIVE_DOCTOR_JSON" |
    PRE_ENV="$1" PRE_REQUIRED="${2:-1}" PRE_STATE_DIR="${3:-}" python3 -I -c "$LIVE_PREFLIGHT_PY" || {
      echo "FAIL 0: preflight" >&2
      exit 1
    }
}

live_listing() {
  local env_name="$1" name="$2" out
  out="$("$THEGN_BIN" "${LIVE_THEGN_ARGS[@]+"${LIVE_THEGN_ARGS[@]}"}" fleet list --env "$env_name" --json 2>/dev/null)" || {
    echo unknown
    return 0
  }
  printf '%s' "$out" | LIVE_NAME="$name" python3 -I -c '
import json, os, sys

try:
    listing = json.load(sys.stdin)
    view = listing["view"]
    rows = view["rows"]
    if not isinstance(view, dict) or not isinstance(rows, list):
        raise ValueError
except (AttributeError, KeyError, TypeError, ValueError):
    print("unknown")
    sys.exit(0)

name = os.environ["LIVE_NAME"]
for row in rows:
    if isinstance(row, dict) and row.get("id") == name:
        state = row.get("state")
        print("present " + (state if isinstance(state, str) else "-"))
        sys.exit(0)
if view.get("partial"):
    print("unknown")
else:
    print("absent")
'
}
