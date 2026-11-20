#!/usr/bin/env bash
# scripts/spikes/lib-cases.sh — JSON-lines case runner for on-device spikes
# (WS05-P0-01, WS05-P0-06, and anything else that wants one evidence line per
# check). Source AFTER lib.sh. bash 3.2 safe (stock macOS).
#
#   sc_init ITEM OUTDIR        evidence file is OUTDIR/ITEM.evidence.jsonl
#   sc_case ID EXPECT cmd...   run cmd (timeout $SC_TIMEOUT, default 30 s), append one
#                              JSON line: {id,macos,arch,thegn_commit,cmd,rc,expect,
#                              stdout_sha,stdout_head,stderr_head,verdict}
#                              EXPECT: rc0 | rcnz | <integer rc> | any
#                              verdict: pass | fail | recorded | skipped-not-installed
#                              SC_NEEDS=tool overrides the presence check (default: argv[0]).
#                              Raw output is saved as OUTDIR/ITEM/ID.out and ID.err.
#   sc_skip ID REASON          record a skipped line without running anything
#   sc_verdict TEXT            final line, also echoed to stdout

SC_COMMIT="unknown"

sc_json() { # sc_json STRING -> quoted JSON string
  printf '%s' "$1" | sp_json_str
}

sc_init() {
  umask 077
  SP_ITEM="$1"
  [ -n "${2:-}" ] || sp_die "usage: $0 OUTDIR   (see docs/superpowers/specs/sandbox-remote-spikes/${SP_ITEM}.md)"
  SP_OUT="$2"
  mkdir -p "$SP_OUT/$SP_ITEM"
  SP_EVID="$SP_OUT/$SP_ITEM.evidence.jsonl"
  python3 - "$SP_OUT/$SP_ITEM" <<'PY' || sp_die "evidence directory must be owned by this user and private (mode 0700)"
import os, stat, sys
p = os.stat(sys.argv[1], follow_symlinks=False)
if not stat.S_ISDIR(p.st_mode) or p.st_uid != os.getuid() or (p.st_mode & 0o077):
    raise SystemExit(1)
PY
  [ ! -e "$SP_EVID" ] && [ ! -L "$SP_EVID" ] || sp_die "evidence already exists: $SP_EVID (choose a new OUTDIR)"
  : >"$SP_EVID"
  SC_COMMIT="$(git -C "$(dirname "${BASH_SOURCE[0]}")/../.." rev-parse HEAD 2>/dev/null || echo unknown)"
  SC_MACOS="$(sw_vers -productVersion 2>/dev/null || uname -sr)"
  SC_ARCH="$(uname -m)"
  SC_HOST="$(hostname 2>/dev/null || true)"
  export SP_REDACT_USER="${USER:-$(id -un)}" SP_REDACT_HOST="$SC_HOST"
  script_path="$(printf '%s' "$0" | sp_redact)"
  printf '{"id":"_meta","item":"%s","date_utc":"%s","macos":%s,"arch":"%s","thegn_commit":"%s","script":%s}\n' \
    "$SP_ITEM" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(sc_json "$SC_MACOS")" "$SC_ARCH" "$SC_COMMIT" "$(sc_json "$script_path")" >>"$SP_EVID"
}

sc_line() { # sc_line ID CMD RC EXPECT SHA OUT ERR VERDICT
  local command_text stdout_text stderr_text
  command_text="$(printf '%s' "$2" | sp_redact)"
  stdout_text="$(printf '%s' "$6" | sp_redact)"
  stderr_text="$(printf '%s' "$7" | sp_redact)"
  printf '{"id":"%s","macos":%s,"arch":"%s","thegn_commit":"%s","cmd":%s,"rc":%s,"expect":"%s","stdout_sha":"%s","stdout_head":%s,"stderr_head":%s,"verdict":"%s"}\n' \
    "$1" "$(sc_json "$SC_MACOS")" "$SC_ARCH" "$SC_COMMIT" "$(sc_json "$command_text")" "$3" "$4" "$5" "$(sc_json "$stdout_text")" "$(sc_json "$stderr_text")" "$8" >>"$SP_EVID"
}

sc_skip() {
  sc_line "$1" "" -1 skipped "" "" "$2" skipped-not-installed
}

sc_case() {
  local id="$1" expect="$2" needs rc=0 sha verdict out err base
  shift 2
  needs="${SC_NEEDS:-$1}"
  base="$SP_OUT/$SP_ITEM/$id"
  if ! command -v "$needs" >/dev/null 2>&1; then
    sc_line "$id" "$*" -1 "$expect" "" "" "not installed: $needs" skipped-not-installed
    return 0
  fi
  sp_timeout "${SC_TIMEOUT:-30}" "$@" >"$base.out" 2>"$base.err" </dev/null || rc=$?
  sp_redact <"$base.out" >"$base.out.r" && mv "$base.out.r" "$base.out"
  sp_redact <"$base.err" >"$base.err.r" && mv "$base.err.r" "$base.err"
  sha="$(shasum -a 256 <"$base.out" 2>/dev/null | cut -d' ' -f1)"
  case "$expect" in
  rc0) if [ "$rc" -eq 0 ]; then verdict=pass; else verdict=fail; fi ;;
  rcnz) if [ "$rc" -ne 0 ]; then verdict=pass; else verdict=fail; fi ;;
  any) verdict=recorded ;;
  *) if [ "$rc" -eq "$expect" ]; then verdict=pass; else verdict=fail; fi ;;
  esac
  if [ "${SC_SUMMARY_PRIVATE:-0}" = 1 ]; then
    out="[private raw capture omitted from JSONL]"
    err="[private raw capture omitted from JSONL]"
  else
    out="$(head -c 400 "$base.out")"
    err="$(head -c 400 "$base.err")"
  fi
  sc_line "$id" "$*" "$rc" "$expect" "$sha" "$out" "$err" "$verdict"
  # shellcheck disable=SC2034 # read by the sourcing scripts
  SC_LAST_RC=$rc
  # shellcheck disable=SC2034
  SC_LAST_VERDICT=$verdict
  return 0
}

sc_verdict() {
  printf '{"id":"_verdict","verdict":%s}\n' "$(sc_json "$1")" >>"$SP_EVID"
  printf 'VERDICT (%s): %s\n' "$SP_ITEM" "$1"
}
