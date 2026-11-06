#!/usr/bin/env bash
set -euo pipefail

OUT="${TMPDIR:-/tmp}/thegn-demo-out"
BIN="${THEGN_BIN:-target/release/thegn}"
SIZE=120x32
SCRIPT_DIR="$(cd "${BASH_SOURCE[0]%/*}" && pwd)"
STEPS="$SCRIPT_DIR/steps.tsv"
NO_PROMPT=0
DRY_RUN=0

fail() {
  printf 'demo: %s\n' "$*" >&2
  exit 2
}

while (($#)); do
  case "$1" in
  --out)
    (($# >= 2)) || fail "--out needs a directory"
    OUT=$2
    shift 2
    ;;
  --bin)
    (($# >= 2)) || fail "--bin needs a path"
    BIN=$2
    shift 2
    ;;
  --size)
    (($# >= 2)) || fail "--size needs WxH"
    SIZE=$2
    shift 2
    ;;
  --steps)
    (($# >= 2)) || fail "--steps needs a file"
    STEPS=$2
    shift 2
    ;;
  --no-prompt)
    NO_PROMPT=1
    shift
    ;;
  --dry-run)
    DRY_RUN=1
    shift
    ;;
  *)
    fail "unknown option: $1"
    ;;
  esac
done

[[ -r $STEPS ]] || fail "cannot read steps file: $STEPS"

if ((DRY_RUN)); then
  total=0
  while IFS=$'\t' read -r kind arg note || [[ -n "${kind:-}${arg:-}${note:-}" ]]; do
    [[ -z ${kind:-} || $kind == \#* ]] && continue
    ((total += 1))
  done <"$STEPS"
  index=0
  while IFS=$'\t' read -r kind arg note || [[ -n "${kind:-}${arg:-}${note:-}" ]]; do
    [[ -z ${kind:-} || $kind == \#* ]] && continue
    ((index += 1))
    printf 'demo: step %d/%d %s %s\n' "$index" "$total" "$kind" "${arg:--}"
  done <"$STEPS"
  exit 0
fi

mkdir -p "$OUT" || fail "cannot create output directory: $OUT"
command -v muse >/dev/null 2>&1 || fail "muse is required on PATH"
command -v git >/dev/null 2>&1 || fail "git is required on PATH"
[[ -x $BIN ]] || fail "built binary is not executable: $BIN (build it first: just release)"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

T="$(mktemp -d)" || fail "could not create the isolated fixture"
trap 'rm -rf "$T"; exit 129' HUP
trap 'rm -rf "$T"; exit 130' INT
trap 'rm -rf "$T"; exit 143' TERM
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/home" "$T/cfg/thegn" "$T/state" "$T/run" "$T/bin" || fail "could not create fixture directories"
chmod 700 "$T/run" || fail "could not protect the runtime directory"
printf '#!/bin/sh\nexport PS1="$ "\nexec /bin/sh --norc --noprofile -i\n' >"$T/bin/e2esh" || fail "could not write shell wrapper"
chmod +x "$T/bin/e2esh" || fail "could not make shell wrapper executable"
printf '[sandbox]\nbackend = "none"\n[media]\nenabled = false\n' >"$T/cfg/thegn/config.toml" || fail "could not write isolated config"
printf '[user]\nname = demo\nemail = demo@example.invalid\n' >"$T/gitconfig" || fail "could not write isolated git config"

export HOME="$T/home"
export XDG_CONFIG_HOME="$T/cfg"
export XDG_STATE_HOME="$T/state"
export XDG_RUNTIME_DIR="$T/run"
export GIT_CONFIG_GLOBAL="$T/gitconfig"
export GIT_CONFIG_SYSTEM=/dev/null
export DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null/e2e-no-dbus
export THEGN_SKIP_ONBOARDING=1
export MUSE_READY=1
export TERM=xterm-256color
export SHELL="$T/bin/e2esh"
export MUSE_SOCKET="$T/muse.sock"
unset THEGN_E2E THEGN_NO_DAEMON GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR
printf 'demo: isolated XDG_RUNTIME_DIR=%s\n' "$T/run"

mkdir -p "$T/repo" || fail "could not create fixture repo"
git -C "$T/repo" init -q -b main || fail "could not initialize fixture repo"
printf 'hello\n' >"$T/repo/README.md" || fail "could not write fixture README"
git -C "$T/repo" add README.md || fail "could not stage fixture README"
GIT_AUTHOR_DATE=2020-01-01T00:00:00Z GIT_COMMITTER_DATE=2020-01-01T00:00:00Z \
  git -C "$T/repo" -c commit.gpgsign=false commit -q -m init || fail "could not commit fixture repo"

SESSION_OPEN=0
cleanup_daemon() {
  attempt=0
  while ((attempt < 20)); do
    if output=$("$BIN" daemon stop 2>&1); then
      sockets=$(find "$T/run" -type s -print -quit 2>/dev/null) || return 1
      if [[ $output == *"no thegn pane daemon is running"* && -z $sockets ]]; then
        return 0
      fi
      if [[ -z $sockets ]]; then
        return 0
      fi
    else
      return 1
    fi
    ((attempt += 1))
    sleep 0.5
  done
  return 1
}

on_exit() {
  status=$?
  trap - EXIT HUP INT TERM
  if ((SESSION_OPEN)); then
    if sessions=$(muse session list 2>/dev/null) && [[ $sessions == *demo* ]]; then
      if ! muse session trace demo --out "$OUT/trace"; then
        printf 'demo: could not write final trace\n' >&2
        status=1
      fi
    fi
  fi
  if command -v muse >/dev/null 2>&1; then
    if ! muse session close --all >/dev/null 2>&1; then
      printf 'demo: could not close private muse sessions\n' >&2
      status=1
    fi
  fi
  if ! cleanup_daemon; then
    printf 'demo: daemon cleanup did not confirm the endpoint was removed\n' >&2
    status=1
  fi
  rm -rf "$T" || status=1
  exit "$status"
}
trap on_exit EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

open_session() {
  muse session open --name demo --size "$SIZE" --cwd "$T/repo" \
    --env "HOME=$T/home" \
    --env "XDG_CONFIG_HOME=$T/cfg" \
    --env "XDG_STATE_HOME=$T/state" \
    --env "XDG_RUNTIME_DIR=$T/run" \
    --env "SHELL=$T/bin/e2esh" \
    --env "GIT_CONFIG_GLOBAL=$T/gitconfig" \
    --env GIT_CONFIG_SYSTEM=/dev/null \
    --env DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null/e2e-no-dbus \
    --env THEGN_SKIP_ONBOARDING=1 \
    --env MUSE_READY=1 \
    --env TERM=xterm-256color \
    -- "$BIN" || fail "muse could not open the thegn session"
  SESSION_OPEN=1
  muse session wait demo --visible NORMAL --timeout-ms 20000 || fail "thegn did not reach its first frame"
}

total=0
while IFS=$'\t' read -r kind arg note || [[ -n "${kind:-}${arg:-}${note:-}" ]]; do
  [[ -z ${kind:-} || $kind == \#* ]] && continue
  ((total += 1))
done <"$STEPS"

index=0
while IFS=$'\t' read -r kind arg note || [[ -n "${kind:-}${arg:-}${note:-}" ]]; do
  [[ -z ${kind:-} || $kind == \#* ]] && continue
  ((index += 1))
  printf 'demo: step %d/%d %s %s\n' "$index" "$total" "$kind" "${arg:--}"
  case "$kind" in
  wait)
    muse session wait demo --visible "$arg" --timeout-ms "${note:-15000}" || fail "wait timed out on: $arg"
    ;;
  key)
    muse session send demo --key "$arg" || fail "could not send key: $arg"
    ;;
  text)
    muse session send demo --text "$arg" --key enter || fail "could not send text"
    ;;
  pause)
    sleep "$(awk "BEGIN { printf \"%.3f\", $arg / 1000 }")" || fail "pause failed"
    ;;
  snap)
    muse session snap demo >"$OUT/$arg.txt" || fail "could not save snapshot: $arg"
    ;;
  detach)
    muse session trace demo --out "$OUT/trace-before-detach" || fail "could not save pre-detach trace"
    muse session send demo --key ctrl+q || fail "could not detach thegn"
    poll=0
    while ((poll < 20)); do
      sessions=$(muse session list) || fail "could not inspect muse sessions after detach"
      [[ $sessions != *demo* ]] && break
      ((poll += 1))
      sleep 0.5
    done
    [[ $sessions != *demo* ]] || fail "thegn did not detach within 10 seconds"
    SESSION_OPEN=0
    ;;
  reattach)
    open_session
    ;;
  manual)
    if ((NO_PROMPT)); then
      printf 'demo: manual: %s\n' "$arg"
    else
      printf 'demo: manual: %s\nPress Enter when ready. ' "$arg"
      read -r
    fi
    ;;
  cleanup)
    cleanup_daemon || fail "daemon cleanup did not confirm the endpoint was removed"
    ;;
  *)
    fail "unknown step kind: $kind"
    ;;
  esac
done <"$STEPS"

printf 'demo: output directory %s\n' "$OUT"
printf 'demo: asciinema cast %s\n' "$OUT/trace/output.cast"
