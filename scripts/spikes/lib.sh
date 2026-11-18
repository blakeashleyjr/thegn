#!/usr/bin/env bash
# scripts/spikes/lib.sh — shared helpers for the sandbox + remote programme
# wave-0 spike scripts (docs/superpowers/specs/sandbox-remote-spikes/).
#
# Source it; do not run it. Written for bash 3.2 (stock macOS): no mapfile, no
# associative arrays, no ${var,,}. The sourcing script sets `set -euo pipefail`.
#
# Contract for every spike script:
#   $1            output directory (created; the ONLY place anything is written)
#   evidence file $1/<ITEM>.evidence.txt (plain text), raw captures under
#                 $1/<ITEM>/ only where a procedure asks for fixtures
#   exit status   non-zero on a failed precondition; 0 otherwise, with the
#                 decision-rule verdict (when computable) on the last lines
#   cleanup       register with sp_on_exit; the trap runs on every exit path
#   secrets       never embedded; read from named environment variables only

SP_CLEANUPS=()
SP_ITEM=""
SP_OUT=""
SP_EVID=""
# shellcheck disable=SC2034 # read by the sourcing scripts
SP_RC=0 # rc of the last sp_cap

sp_die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

# sp_on_exit 'command string' — run LIFO when the script exits for any reason.
sp_on_exit() {
  SP_CLEANUPS+=("$1")
}

sp_run_cleanups() {
  local i
  i=${#SP_CLEANUPS[@]}
  while [ "$i" -gt 0 ]; do
    i=$((i - 1))
    # best-effort: cleanup must never mask the script's own exit status
    eval "${SP_CLEANUPS[$i]}" >/dev/null 2>&1 || true
  done
}
trap sp_run_cleanups EXIT

# sp_init ITEM OUTDIR — create the output dir and the evidence file header.
sp_init() {
  SP_ITEM="$1"
  [ -n "${2:-}" ] || sp_die "usage: $0 OUTDIR   (see docs/superpowers/specs/sandbox-remote-spikes/${SP_ITEM}.md)"
  SP_OUT="$2"
  mkdir -p "$SP_OUT/$SP_ITEM"
  SP_EVID="$SP_OUT/$SP_ITEM.evidence.txt"
  : >"$SP_EVID"
  sp_note "# $SP_ITEM evidence"
  sp_kv date_utc "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  sp_kv host "$(uname -srm)"
  sp_kv script "$0"
}

sp_need() { # sp_need tool... — precondition: each tool on PATH
  local t
  for t in "$@"; do
    command -v "$t" >/dev/null 2>&1 || sp_die "missing required tool: $t"
  done
}

sp_need_env() { # sp_need_env NAME... — precondition: each variable set, non-empty
  local v
  for v in "$@"; do
    [ -n "${!v:-}" ] || sp_die "environment variable $v is not set (never pass secrets on argv)"
  done
}

# Redact user names and home paths from a stream.
sp_redact() {
  local h u
  h="${HOME:-/nonexistent-home}"
  u="${USER:-$(id -un)}"
  sed -e "s#$h#<HOME>#g" -e "s#/Users/$u#/Users/<USER>#g" -e "s#/home/$u#/home/<USER>#g"
}

sp_note() { printf '%s\n' "$*" >>"$SP_EVID"; }
sp_kv() { printf '%s: %s\n' "$1" "$2" >>"$SP_EVID"; }
sp_say() { printf '%s\n' "$*" >&2; }

# sp_timeout SECONDS cmd args... — portable timeout (stock macOS has none).
# Returns the command's status, or 124 when it was killed for running long.
sp_timeout() {
  local secs="$1" pid watcher rc
  shift
  "$@" &
  pid=$!
  (
    sleep "$secs"
    kill -TERM "$pid" 2>/dev/null
    sleep 2
    kill -KILL "$pid" 2>/dev/null
  ) >/dev/null 2>&1 &
  watcher=$!
  rc=0
  wait "$pid" 2>/dev/null || rc=$?
  kill "$watcher" 2>/dev/null || true
  wait "$watcher" 2>/dev/null || true
  case "$rc" in 143 | 137) rc=124 ;; esac
  return "$rc"
}

# sp_cap NAME SECS cmd args... — run a command with a timeout, append argv, rc and
# redacted stdout/stderr to the evidence file, and save raw output beside it.
# Never fails the script; the exit code is recorded as rc=.
sp_cap() {
  local name="$1" secs="$2" rc f
  shift 2
  f="$SP_OUT/$SP_ITEM/$name"
  rc=0
  sp_timeout "$secs" "$@" >"$f.out" 2>"$f.err" </dev/null || rc=$?
  sp_redact <"$f.out" >"$f.out.r" && mv "$f.out.r" "$f.out"
  sp_redact <"$f.err" >"$f.err.r" && mv "$f.err.r" "$f.err"
  sp_note ""
  sp_note "## $name"
  sp_note "argv: $*"
  sp_note "rc: $rc"
  # shellcheck disable=SC2034
  SP_RC=$rc
  sp_note "--- stdout (head)"
  head -c 2000 "$f.out" >>"$SP_EVID"
  sp_note "--- stderr (head)"
  head -c 2000 "$f.err" >>"$SP_EVID"
  sp_note ""
  return 0
}

# Percentile helper: sp_pctl PCT < numbers-one-per-line  (nearest rank).
sp_pctl() {
  sort -n | awk -v p="$1" '{a[NR]=$1} END{ if(NR==0){print "NA"; exit} i=int((p/100)*NR+0.999999); if(i<1)i=1; if(i>NR)i=NR; print a[i] }'
}

# sp_verdict RULE_TEXT RESULT — print the decision-rule verdict to evidence and stdout.
sp_verdict() {
  sp_note ""
  sp_note "## VERDICT"
  sp_note "rule: $1"
  sp_note "result: $2"
  printf 'VERDICT (%s): %s\n' "$SP_ITEM" "$2"
}

# sp_have_cmd — used for "present or absent" rows.
sp_have() { command -v "$1" >/dev/null 2>&1; }

# sp_json_str — JSON-escape stdin into a quoted string (no jq dependency).
sp_json_str() {
  awk 'BEGIN{ORS=""; printf "\""} {gsub(/\\/,"\\\\"); gsub(/"/,"\\\""); gsub(/\t/,"\\t"); gsub(/\r/,""); if(NR>1)printf "\\n"; printf "%s",$0} END{printf "\""}'
}
