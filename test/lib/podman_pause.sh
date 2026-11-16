#!/usr/bin/env bash
# test/lib/podman_pause.sh — attribute and reap rootless podman `pause`
# processes owned by a harness's private XDG_RUNTIME_DIR (THE-870).
#
# Rootless podman creates a user-namespace `pause` process per runtime dir and
# leaves it running after the command exits. A harness that points podman (or
# a thegn that probes podman) at a throwaway runtime dir leaks one per run, and
# the leak pins the checkout it was started from.
#
# `source` this file; it defines functions only. Safe under `set -u`, and
# every function returns 0 so a `set -e` caller is never aborted by it.
# Attribution mirrors test/podman-pause-bisect.sh (comm, then XDG_RUNTIME_DIR
# from /proc/<pid>/environ). Only pids attributed to the given dir are ever
# signalled.

# podman_pause_pids_for <runtime-dir>: print pids whose comm is `podman pause`
# or `pause` and whose XDG_RUNTIME_DIR equals <runtime-dir> or lies beneath it.
# Prints nothing where /proc is unavailable (non-Linux).
podman_pause_pids_for() {
  local dir="${1:-}" d pid comm entry rd
  dir="${dir%/}"
  [[ -n $dir ]] || return 0
  [[ -r /proc/self/comm ]] || return 0
  for d in /proc/[0-9]*; do
    [[ -d $d ]] || continue
    comm=""
    IFS= read -r comm 2>/dev/null <"$d/comm" || true
    [[ $comm == 'podman pause' || $comm == pause ]] || continue
    pid="${d##*/}"
    rd=""
    if [[ -r "$d/environ" ]]; then
      while IFS= read -r -d '' entry; do
        if [[ ${entry%%=*} == XDG_RUNTIME_DIR ]]; then
          rd="${entry#*=}"
          break
        fi
      done 2>/dev/null <"$d/environ" || true
    fi
    rd="${rd%/}"
    [[ -n $rd ]] || continue
    if [[ $rd == "$dir" || $rd == "$dir"/* ]]; then
      printf '%s\n' "$pid"
    fi
  done
  return 0
}

# podman_pause_reap <runtime-dir>: terminate every pause process attributed to
# <runtime-dir>. The `libpod/tmp/pause.pid` pid goes first (only when it is one
# of the attributed pids), then every other attributed pid gets TERM; after up
# to 2 s the survivors get KILL. Never signals a pid outside <runtime-dir>.
podman_pause_reap() {
  local dir="${1:-}" pid_file file_pid="" pid alive tries
  local -a pids=() survivors=()
  dir="${dir%/}"
  [[ -n $dir ]] || return 0
  while IFS= read -r pid; do
    [[ -n $pid ]] && pids+=("$pid")
  done < <(podman_pause_pids_for "$dir")
  [[ ${#pids[@]} -gt 0 ]] || return 0

  pid_file="$dir/libpod/tmp/pause.pid"
  if [[ -r $pid_file ]]; then
    IFS= read -r file_pid 2>/dev/null <"$pid_file" || true
  fi
  if [[ -n $file_pid ]]; then
    for pid in "${pids[@]}"; do
      if [[ $pid == "$file_pid" ]]; then
        kill -TERM "$pid" 2>/dev/null || true
        break
      fi
    done
  fi
  for pid in "${pids[@]}"; do
    [[ $pid == "$file_pid" ]] && continue
    kill -TERM "$pid" 2>/dev/null || true
  done

  # One 2 s budget for the TERM grace period; KILL is not followed by a wait.
  for ((tries = 0; tries < 20; tries++)); do
    alive=0
    for pid in "${pids[@]}"; do
      if kill -0 "$pid" 2>/dev/null; then
        alive=1
        break
      fi
    done
    [[ $alive -eq 1 ]] || return 0
    sleep 0.1
  done
  # Re-attribute rather than trusting pids from 2 s ago: a reused pid must
  # never be KILLed.
  while IFS= read -r pid; do
    [[ -n $pid ]] && survivors+=("$pid")
  done < <(podman_pause_pids_for "$dir")
  if [[ ${#survivors[@]} -gt 0 ]]; then
    kill -KILL "${survivors[@]}" 2>/dev/null || true
  fi
  return 0
}
