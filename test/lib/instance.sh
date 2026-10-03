#!/bin/sh
# Instance lifecycle for the developer launch recipes (`just start*`).
#
# A pid file alone is not an identity: the integer can be recycled by any
# unrelated process of the same user. So the metadata recorded is
# `<pid> <start-generation>` and nothing is signalled unless BOTH still match
# the live process (the start generation is the kernel's process start time,
# which a reused pid cannot share). Malformed metadata is never signalled.
# Daemon / detached-pane rotation is scoped to one state root by checking each
# candidate's own XDG_STATE_HOME, never by a user-wide command-line regex.
#
# POSIX sh (sourced by `just` recipes). Source it, then:
#   inst_restart  PIDFILE            # lock, stop the prior generation, bounded
#   inst_write_meta PIDFILE PID      # atomically record PID's generation
#   inst_rotate_scoped STATE_ROOT    # stop this root's daemon + dtach panes

# Start generation of PID, or empty when it does not exist.
inst_gen() {
  _ig_pid=$1
  if [ -r "/proc/$_ig_pid/stat" ]; then
    # Field 22 overall; comm (field 2) may contain spaces, so cut after ")".
    # best-effort: a vanished process just yields an empty generation.
    sed -e 's/^.*) //' "/proc/$_ig_pid/stat" 2>/dev/null | awk '{print $20}'
  else
    ps -o lstart= -p "$_ig_pid" 2>/dev/null | tr -s ' ' '_' | sed 's/^_//;s/_$//'
  fi
}

# Strict parse: sets INST_PID / INST_GEN or returns 1. Refuses symlinks,
# non-regular files, oversize files, multi-line / partial / zero / negative /
# overflowing / non-numeric values.
inst_parse_meta() {
  INST_PID=
  INST_GEN=
  [ -f "$1" ] && [ ! -L "$1" ] || return 1
  [ "$(wc -c <"$1" | tr -d ' ')" -le 128 ] || return 1
  [ "$(wc -l <"$1" | tr -d ' ')" -le 1 ] || return 1
  _ip_line=$(cat "$1")
  _ip_pid=${_ip_line%% *}
  _ip_gen=${_ip_line#* }
  [ "$_ip_pid" != "$_ip_line" ] || return 1
  case $_ip_pid in
  '' | *[!0-9]* | 0*) return 1 ;;
  esac
  [ ${#_ip_pid} -le 7 ] && [ "$_ip_pid" -le 4194304 ] || return 1
  case $_ip_gen in
  '' | *[!0-9A-Za-z:_]*) return 1 ;;
  esac
  INST_PID=$_ip_pid
  INST_GEN=$_ip_gen
}

# Atomically write `PID GEN` (private tmp in the same dir, then rename).
inst_write_meta() {
  _iw_file=$1
  _iw_pid=$2
  _iw_gen=$(inst_gen "$_iw_pid")
  [ -n "$_iw_gen" ] || return 1
  [ ! -d "$_iw_file" ] || return 1
  # mktemp creates 0600 and exclusively, so a planted path is never followed.
  _iw_tmp=$(mktemp "$_iw_file.XXXXXX") || return 1
  printf '%s %s\n' "$_iw_pid" "$_iw_gen" >"$_iw_tmp" || {
    rm -f "$_iw_tmp"
    return 1
  }
  mv -f "$_iw_tmp" "$_iw_file"
}

# True while PID is still the recorded generation.
inst_same() { [ "$(inst_gen "$1")" = "$2" ]; }

# Signal PID only if it is still generation GEN; wait up to GRACE seconds, then
# escalate to KILL (re-verifying identity), then confirm it is gone.
inst_stop_gen() {
  _is_pid=$1
  _is_gen=$2
  _is_grace=${3:-5}
  inst_same "$_is_pid" "$_is_gen" || return 0
  kill "$_is_pid" 2>/dev/null || true
  _is_n=0
  while [ "$_is_n" -lt $((_is_grace * 10)) ]; do
    inst_same "$_is_pid" "$_is_gen" || return 0
    sleep 0.1
    _is_n=$((_is_n + 1))
  done
  if inst_same "$_is_pid" "$_is_gen"; then
    kill -9 "$_is_pid" 2>/dev/null || true
    _is_n=0
    while [ "$_is_n" -lt 20 ]; do
      inst_same "$_is_pid" "$_is_gen" || return 0
      sleep 0.1
      _is_n=$((_is_n + 1))
    done
    echo "instance: pid $_is_pid ($_is_gen) survived SIGKILL; not relaunching over it" >&2
    return 1
  fi
}

# Stop the generation recorded in PIDFILE. Stale state (pid gone or reused) is
# removed only after proving it is not the recorded generation; malformed
# state is left untouched and never signalled.
inst_stop() {
  [ -e "$1" ] || [ -L "$1" ] || return 0
  if ! inst_parse_meta "$1"; then
    echo "instance: ignoring malformed pidfile $1 (not signalling)" >&2
    return 0
  fi
  inst_stop_gen "$INST_PID" "$INST_GEN" "${2:-5}" || return 1
  inst_same "$INST_PID" "$INST_GEN" || rm -f "$1"
}

# Serialize launchers of the same state root around stop (+ optional record of
# pid WRITEPID). The lock fd is confined to the subshell so the exec'd instance
# never inherits it.
inst_restart() {
  _ir_file=$1
  _ir_write=${2:-}
  _ir_lock="$_ir_file.lock"
  (
    if command -v flock >/dev/null 2>&1; then
      exec 9>"$_ir_lock"
      flock -w 15 9 || {
        echo "instance: another launcher holds $_ir_lock" >&2
        exit 1
      }
    fi
    inst_stop "$_ir_file" || exit 1
    if [ -n "$_ir_write" ]; then inst_write_meta "$_ir_file" "$_ir_write"; fi
  )
}

# Stop this state root's release daemon and detached dtach pane shells (the
# "reattaches OLD-binary pane sessions" trap) without touching any other
# checkout, fixture or differently named instance. Linux /proc only; elsewhere
# it refuses to guess.
inst_rotate_scoped() {
  _ir_state=$1
  [ -d /proc/self ] || {
    echo "instance: scoped rotation needs /proc; skipping" >&2
    return 0
  }
  for _ir_d in /proc/[0-9]*; do
    _ir_p=${_ir_d#/proc/}
    [ "$_ir_p" != "$$" ] || continue
    _ir_cmd=$({ cat "$_ir_d/cmdline" 2>/dev/null || true; } | tr '\0' ' ')
    case $_ir_cmd in
    */thegn\ daemon* | thegn\ daemon* | */tg\ daemon* | tg\ daemon*) ;;
    dtach\ -A\ */tg-socket-* | */dtach\ -A\ */tg-socket-*) ;;
    *) continue ;;
    esac
    { cat "$_ir_d/environ" 2>/dev/null || true; } | tr '\0' '\n' | grep -qxF "XDG_STATE_HOME=$_ir_state" || continue
    _ir_gen=$(inst_gen "$_ir_p")
    [ -n "$_ir_gen" ] || continue
    inst_stop_gen "$_ir_p" "$_ir_gen" 5 || true
  done
}
