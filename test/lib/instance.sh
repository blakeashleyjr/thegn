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
    LC_ALL=C TZ=UTC ps -o lstart= -p "$_ig_pid" 2>/dev/null | tr -s ' ' '_' | sed 's/^_//;s/_$//'
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

# Reads NUL-split env lines on stdin; true if XDG_STATE_HOME is exactly $1.
# (awk consumes all input, so the writer never takes SIGPIPE mid-read.)
inst_has_state() {
  awk -v want="XDG_STATE_HOME=$1" '$0 == want { f = 1 } END { exit !f }'
}

# True when PID is a thegn of state root STATE (Linux /proc only; elsewhere it
# cannot verify and says no).
inst_is_ours() {
  _io_pid=$1
  _io_state=$2
  [ -n "$_io_state" ] && [ -d /proc/self ] || return 1
  _io_argv0=$(tr '\0' '\n' <"/proc/$_io_pid/cmdline" 2>/dev/null | sed -n 1p) || return 1
  case ${_io_argv0##*/} in thegn | tg) ;; *) return 1 ;; esac
  tr '\0' '\n' <"/proc/$_io_pid/environ" 2>/dev/null | inst_has_state "$_io_state"
}

# Stop the generation recorded in PIDFILE. Stale state (pid gone or reused) is
# removed only after proving it is not the recorded generation. A legacy bare
# `<pid>` file is honoured only if that process is verifiably thegn of STATE
# (third arg); any other malformed file naming a LIVE process refuses the
# launch so a second instance never starts on the same state root.
inst_stop() {
  [ -e "$1" ] || [ -L "$1" ] || return 0
  if ! inst_parse_meta "$1"; then
    _is_tok=
    if [ -f "$1" ] && [ ! -L "$1" ]; then
      _is_tok=$(head -c 64 "$1" 2>/dev/null | tr ' ' '\n' | sed -n 1p)
    fi
    case $_is_tok in
    '' | *[!0-9]* | 0*) ;;
    *)
      if [ ${#_is_tok} -le 7 ] && kill -0 "$_is_tok" 2>/dev/null; then
        _is_lines=$(wc -l <"$1" | tr -d ' ')
        _is_words=$(wc -w <"$1" | tr -d ' ')
        if [ "$_is_lines" -le 1 ] && [ "$_is_words" -eq 1 ] && inst_is_ours "$_is_tok" "${3:-${INST_STATE:-}}"; then
          inst_stop_gen "$_is_tok" "$(inst_gen "$_is_tok")" "${2:-5}" || return 1
          kill -0 "$_is_tok" 2>/dev/null || rm -f "$1"
          return 0
        fi
        echo "instance: $1 is malformed and names live pid $_is_tok that is not verifiably this state root's thegn; refusing to launch (stop it manually and rm $1)" >&2
        return 1
      fi
      ;;
    esac
    echo "instance: ignoring malformed pidfile $1 (not signalling)" >&2
    return 0
  fi
  inst_stop_gen "$INST_PID" "$INST_GEN" "${2:-5}" || return 1
  inst_same "$INST_PID" "$INST_GEN" || rm -f "$1"
}

# Run COMMAND... under the launch lock of PIDFILE (no-op lock without flock).
# The fd is confined to the subshell so an exec'd instance never inherits it.
inst_locked() {
  _il_lock="$1.lock"
  shift
  (
    if command -v flock >/dev/null 2>&1; then
      exec 9>"$_il_lock"
      flock -w 15 9 || {
        echo "instance: another launcher holds $_il_lock" >&2
        exit 1
      }
    fi
    "$@"
  )
}

# Record PID's generation under the launch lock; loud on failure.
inst_record() {
  inst_locked "$1" inst_write_meta "$1" "$2" || {
    echo "instance: could not record pidfile $1" >&2
    return 1
  }
}

# Serialize launchers of the same state root around stop (+ optional record of
# pid WRITEPID). INST_STATE (the state root) enables legacy-file verification.
inst_restart() {
  inst_locked "$1" inst_restart_locked "$1" "${2:-}"
}
inst_restart_locked() {
  inst_stop "$1" 5 "${INST_STATE:-}" || return 1
  if [ -n "$2" ]; then inst_write_meta "$1" "$2"; fi
}

# Stop this state root's release daemon (the "reattaches OLD-binary pane
# sessions" trap) without touching any other checkout, fixture or differently
# named instance. Matches `thegn daemon` by argv[0], or by /proc/PID/exe for
# daemons respawned via /proc/self/exe (util::self_exe after a rebuild-in-place,
# exe reads `.../thegn (deleted)`). dtach is deliberately not matched: it only
# runs on remote hosts (placement.rs), never under this local state root.
# Linux /proc only; elsewhere it refuses to guess.
inst_rotate_scoped() {
  _ir_state=$1
  [ -d /proc/self ] || {
    echo "instance: scoped rotation needs /proc; skipping" >&2
    return 0
  }
  # One grep prunes to processes with a `daemon` argument; only those pay for
  # the per-process reads below.
  # shellcheck disable=SC2013 # /proc paths never contain whitespace
  for _ir_c in $(grep -alF daemon /proc/[0-9]*/cmdline 2>/dev/null); do
    _ir_d=${_ir_c%/cmdline}
    _ir_p=${_ir_d#/proc/}
    [ "$_ir_p" != "$$" ] || continue
    _ir_argv=$(tr '\0' '\n' <"$_ir_c" 2>/dev/null | sed -n '1,2p') || continue
    _ir_a0=$(printf '%s\n' "$_ir_argv" | sed -n 1p)
    _ir_a1=$(printf '%s\n' "$_ir_argv" | sed -n 2p)
    [ "$_ir_a1" = daemon ] || continue
    _ir_exe=$(readlink "$_ir_d/exe" 2>/dev/null || true)
    _ir_exe=${_ir_exe% (deleted)}
    case ${_ir_a0##*/} in thegn | tg) ;; *)
      case ${_ir_exe##*/} in thegn | tg) ;; *) continue ;; esac
      ;;
    esac
    tr '\0' '\n' <"$_ir_d/environ" 2>/dev/null | inst_has_state "$_ir_state" || continue
    _ir_gen=$(inst_gen "$_ir_p")
    [ -n "$_ir_gen" ] || continue
    inst_stop_gen "$_ir_p" "$_ir_gen" 5 || true
  done
}
