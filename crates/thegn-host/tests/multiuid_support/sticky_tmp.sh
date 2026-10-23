#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

inner_main() {
  mount -t tmpfs -o mode=1777 tmpfs /tmp 2>/dev/null || {
    printf 'tmpfs mount on /tmp failed\n' >&2
    exit 77
  }
  if [[ $(stat -f -c '%T' /tmp) != tmpfs || $(stat -c '%a' /tmp) != 1777 ]]; then
    printf '/tmp is not a mode-1777 tmpfs\n' >&2
    exit 77
  fi
  if ! setpriv --reuid=1001 --regid=1001 --clear-groups -- true; then
    printf 'setpriv uid 1001 probe failed\n' >&2
    exit 77
  fi
  if ! setpriv --reuid=1002 --regid=1002 --clear-groups -- true; then
    printf 'setpriv uid 1002 probe failed\n' >&2
    exit 77
  fi

  printf 'MULTIUID-READY\n'
  install -d -m 0755 /tmp/tg-bin
  cat <&3 >/tmp/tg-bin/thegn
  chmod 0755 /tmp/tg-bin/thegn
  exec 3<&-
  exec 4<&-
  declare -a DAEMON_PIDS=()

  # shellcheck disable=SC2329 # The EXIT trap invokes this cleanup handler indirectly.
  cleanup() {
    local pid remaining
    for pid in "${DAEMON_PIDS[@]:-}"; do
      if kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null || true
      fi
    done
    for pid in "${DAEMON_PIDS[@]:-}"; do
      remaining=0
      while kill -0 "$pid" 2>/dev/null && ((remaining < 50)); do
        sleep 0.1
        remaining=$((remaining + 1))
      done
      if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
      fi
      wait "$pid" 2>/dev/null || true
    done
  }
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  trap 'exit 129' HUP

  attacker() {
    setpriv --reuid=1001 --regid=1001 --clear-groups -- "$@"
  }

  victim() {
    local home=$1
    shift
    setpriv --reuid=1002 --regid=1002 --clear-groups -- env -i \
      PATH="$PATH" LANG=C HOME="$home" \
      XDG_STATE_HOME="$home/state" XDG_CONFIG_HOME="$home/config" \
      "$@"
  }

  doctor_endpoint() {
    local home=$1 json
    json=$(victim "$home" /tmp/tg-bin/thegn doctor --json) || return 1
    printf '%s\n' "$json" |
      sed -n 's/.*"endpoint"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
      head -n 1
  }

  start_daemon() {
    local home=$1 log=$2
    setpriv --reuid=1002 --regid=1002 --clear-groups -- env -i \
      PATH="$PATH" LANG=C HOME="$home" \
      XDG_STATE_HOME="$home/state" XDG_CONFIG_HOME="$home/config" \
      /tmp/tg-bin/thegn daemon >"$log" 2>&1 &
    CURRENT_PID=$!
    DAEMON_PIDS+=("$CURRENT_PID")
  }

  stop_daemon() {
    local home=$1 pid=$2 stop_output remaining tracked
    local -a active_pids=()
    stop_output=$(victim "$home" /tmp/tg-bin/thegn daemon stop) ||
      fail "daemon stop failed for $home: $stop_output"
    [[ $stop_output == *'stopped '* ]] || fail "daemon stop did not report stopped: $stop_output"
    remaining=0
    while kill -0 "$pid" 2>/dev/null && ((remaining < 100)); do
      sleep 0.1
      remaining=$((remaining + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
      fail "daemon pid $pid did not exit within 10 seconds"
    fi
    wait "$pid" 2>/dev/null || true
    for tracked in "${DAEMON_PIDS[@]}"; do
      if [[ $tracked != "$pid" ]]; then
        active_pids+=("$tracked")
      fi
    done
    DAEMON_PIDS=("${active_pids[@]}")
  }

  assert_attacker_unchanged() {
    local kind=$1 object=$2 before_stat=$3 before_listing=$4
    local after_stat after_listing
    after_stat=$(stat -c '%u %g %a %F %i' -- "$object") ||
      fail "$kind attacker object disappeared: $object"
    if [[ $kind == symlink ]]; then
      after_listing=$(ls -A -d -- "$object") || fail "$kind attacker listing failed: $object"
    else
      after_listing=$(ls -A -- "$object") || fail "$kind attacker listing failed: $object"
    fi
    [[ $after_stat == "$before_stat" ]] ||
      fail "$kind attacker metadata changed: before=[$before_stat] after=[$after_stat]"
    [[ $after_listing == "$before_listing" ]] ||
      fail "$kind attacker listing changed: before=[$before_listing] after=[$after_listing]"
  }

  verify_relocated_endpoint() {
    local kind=$1 endpoint=$2 key=$3 predicted=$4
    [[ $endpoint != "$predicted" ]] || fail "$kind daemon kept the hostile predictable endpoint"
    [[ $endpoint =~ ^/tmp/thegn-1002-${key}-[0-9a-f]{32}/daemon\.sock$ ]] ||
      fail "$kind relocated endpoint has an invalid shape: $endpoint"
    [[ -S $endpoint ]] || fail "$kind relocated endpoint is not a socket: $endpoint"
    local directory metadata
    directory=${endpoint%/daemon.sock}
    metadata=$(stat -c '%u %a %F' -- "$directory") ||
      fail "$kind relocated directory is missing: $directory"
    [[ $metadata == '1002 700 directory' ]] ||
      fail "$kind endpoint directory has unsafe ownership or mode: $metadata"
    local locator="$ACTIVE_HOME/state/thegn/run/tmp-fallback-dir"
    local located
    located=$(<"$locator") || fail "$kind locator is missing: $locator"
    [[ $located == "${directory##*/}" ]] ||
      fail "$kind locator does not name the daemon directory: $located"
  }

  run_case() {
    local kind=$1
    local home="/tmp/victim-$kind"
    local config_file="$home/config/thegn/config.toml"
    local predicted predicted_key object before_stat before_listing
    local endpoint directory pid endpoint_again endpoint_stale locator located candidate

    ACTIVE_HOME=$home
    install -d -m 0700 "$home/state" "$home/config/thegn"
    cat >"$config_file" <<'CONFIG'
[sandbox]
backend = "none"
[media]
enabled = false
[daemon]
idle_exit_secs = 60
CONFIG
    chown -R 1002:1002 "$home"
    chmod 0700 "$home"
    chmod 0700 "$home/state" "$home/config" "$home/config/thegn"
    chmod 0600 "$config_file"

    predicted=$(doctor_endpoint "$home") || fail "$kind doctor did not report an endpoint"
    [[ $predicted =~ ^/tmp/thegn-1002-([0-9a-f]{32})/daemon\.sock$ ]] ||
      fail "$kind predictable endpoint has an invalid shape: $predicted"
    predicted_key=${BASH_REMATCH[1]}
    object=${predicted%/daemon.sock}
    [[ ! -e $object && ! -L $object ]] || fail "$kind predictable path already exists: $object"

    case "$kind" in
    dir)
      attacker mkdir -m 0755 -- "$object" || fail "attacker could not create directory $object"
      attacker touch -- "$object/canary" || fail "attacker could not create directory canary"
      attacker chmod 0644 "$object/canary" || fail "attacker could not set canary mode"
      ;;
    file)
      attacker touch -- "$object" || fail "attacker could not create file $object"
      attacker chmod 0644 "$object" || fail "attacker could not set file mode"
      ;;
    symlink)
      attacker mkdir -m 0777 -- /tmp/attacker-target || fail 'attacker could not create symlink target'
      attacker chmod 0777 /tmp/attacker-target || fail 'attacker could not set symlink target mode'
      attacker touch -- /tmp/attacker-target/canary || fail 'attacker could not create symlink canary'
      attacker chmod 0644 /tmp/attacker-target/canary || fail 'attacker could not set symlink canary mode'
      attacker ln -s /tmp/attacker-target "$object" || fail "attacker could not create symlink $object"
      ;;
    *)
      fail "unknown hostile object shape: $kind"
      ;;
    esac
    before_stat=$(stat -c '%u %g %a %F %i' -- "$object") || fail "$kind attacker object stat failed"
    if [[ $kind == symlink ]]; then
      before_listing=$(ls -A -d -- "$object") || fail "$kind attacker object listing failed"
    else
      before_listing=$(ls -A -- "$object") || fail "$kind attacker object listing failed"
    fi
    case "$kind" in
    dir)
      [[ $(stat -c '%u %g %a %F' -- "$object") == '1001 1001 755 directory' ]] ||
        fail 'attacker directory did not have the expected uid, gid, and mode'
      ;;
    file)
      [[ $(stat -c '%u %g %a %F' -- "$object") == '1001 1001 644 regular file' ]] ||
        fail 'attacker file did not have the expected uid, gid, and mode'
      ;;
    symlink)
      [[ $(stat -c '%u %g %a %F' -- "$object") == '1001 1001 777 symbolic link' ]] ||
        fail 'attacker symlink did not have the expected uid, gid, and mode'
      [[ $(stat -c '%u %g %a %F' -- /tmp/attacker-target) == '1001 1001 777 directory' ]] ||
        fail 'attacker symlink target did not have the expected uid, gid, and mode'
      ;;
    esac
    local target_before_stat='' target_before_listing='' link_target='' canary_path=''
    local canary_before_stat=''
    if [[ $kind == symlink ]]; then
      link_target=$(readlink -- "$object") || fail 'could not read attacker symlink target'
      target_before_stat=$(stat -c '%u %g %a %F %i' -- /tmp/attacker-target) ||
        fail 'could not stat attacker symlink target'
      target_before_listing=$(ls -A -- /tmp/attacker-target) ||
        fail 'could not list attacker symlink target'
      canary_path=/tmp/attacker-target/canary
    elif [[ $kind == dir ]]; then
      canary_path="$object/canary"
      [[ $before_listing == canary ]] || fail 'attacker directory does not contain exactly its canary'
    fi
    if [[ $kind == symlink ]]; then
      [[ $target_before_listing == canary ]] || fail 'attacker symlink target does not contain exactly its canary'
    fi
    if [[ -n $canary_path ]]; then
      canary_before_stat=$(stat -c '%u %g %a %F %i' -- "$canary_path") ||
        fail "$kind attacker canary stat failed"
    fi

    start_daemon "$home" "/tmp/$kind-daemon.log"
    pid=$CURRENT_PID
    endpoint=''
    locator="$home/state/thegn/run/tmp-fallback-dir"
    local attempt=0
    while ((attempt < 100)); do
      if [[ -r $locator ]]; then
        located=$(<"$locator")
        candidate="/tmp/$located/daemon.sock"
        if [[ -S $candidate ]]; then
          endpoint=$(doctor_endpoint "$home") || fail "$kind doctor did not report an endpoint after socket bind"
          break
        fi
      fi
      sleep 0.1
      attempt=$((attempt + 1))
    done
    [[ -n $endpoint && $endpoint != "$predicted" && -S $endpoint ]] ||
      fail "$kind daemon did not bind a relocated endpoint within 10 seconds"
    verify_relocated_endpoint "$kind" "$endpoint" "$predicted_key" "$predicted"
    assert_attacker_unchanged "$kind" "$object" "$before_stat" "$before_listing"
    if [[ $kind == symlink ]]; then
      [[ $(readlink -- "$object") == "$link_target" ]] || fail 'attacker symlink target changed'
      [[ $(stat -c '%u %g %a %F %i' -- /tmp/attacker-target) == "$target_before_stat" ]] ||
        fail 'attacker symlink target metadata changed'
      [[ $(ls -A -- /tmp/attacker-target) == "$target_before_listing" ]] ||
        fail 'attacker symlink target listing changed'
    fi
    if [[ -n $canary_path ]]; then
      [[ $(stat -c '%u %g %a %F %i' -- "$canary_path") == "$canary_before_stat" ]] ||
        fail "$kind attacker canary metadata changed"
    fi
    stop_daemon "$home" "$pid"
    [[ ! -e $endpoint && ! -L $endpoint ]] || fail "$kind daemon socket remains after stop: $endpoint"
    assert_attacker_unchanged "$kind" "$object" "$before_stat" "$before_listing"
    if [[ $kind == symlink ]]; then
      [[ $(readlink -- "$object") == "$link_target" ]] || fail 'attacker symlink target changed after stop'
      [[ $(stat -c '%u %g %a %F %i' -- /tmp/attacker-target) == "$target_before_stat" ]] ||
        fail 'attacker symlink target metadata changed after stop'
      [[ $(ls -A -- /tmp/attacker-target) == "$target_before_listing" ]] ||
        fail 'attacker symlink target listing changed after stop'
    fi
    if [[ -n $canary_path ]]; then
      [[ $(stat -c '%u %g %a %F %i' -- "$canary_path") == "$canary_before_stat" ]] ||
        fail "$kind attacker canary metadata changed after stop"
    fi
    printf 'PASS: %s hostile entry relocates safely and stops cleanly\n' "$kind"

    start_daemon "$home" "/tmp/$kind-restart.log"
    pid=$CURRENT_PID
    endpoint_again=''
    attempt=0
    while ((attempt < 100)); do
      if [[ -S $endpoint ]]; then
        endpoint_again=$(doctor_endpoint "$home") || fail "$kind doctor did not report the reused endpoint"
        break
      fi
      sleep 0.1
      attempt=$((attempt + 1))
    done
    [[ $endpoint_again == "$endpoint" && -S $endpoint_again ]] ||
      fail "$kind restart did not reuse locator endpoint: $endpoint_again"
    verify_relocated_endpoint "$kind" "$endpoint_again" "$predicted_key" "$predicted"
    stop_daemon "$home" "$pid"
    [[ ! -e $endpoint && ! -L $endpoint ]] || fail "$kind restart socket remains after stop"
    assert_attacker_unchanged "$kind" "$object" "$before_stat" "$before_listing"
    if [[ $kind == symlink ]]; then
      [[ $(readlink -- "$object") == "$link_target" ]] || fail 'attacker symlink target changed after restart stop'
      [[ $(stat -c '%u %g %a %F %i' -- /tmp/attacker-target) == "$target_before_stat" ]] ||
        fail 'attacker symlink target metadata changed after restart stop'
      [[ $(ls -A -- /tmp/attacker-target) == "$target_before_listing" ]] ||
        fail 'attacker symlink target listing changed after restart stop'
    fi
    if [[ -n $canary_path ]]; then
      [[ $(stat -c '%u %g %a %F %i' -- "$canary_path") == "$canary_before_stat" ]] ||
        fail "$kind attacker canary metadata changed after restart stop"
    fi
    printf 'PASS: %s restart reuses the relocated locator\n' "$kind"

    directory=${endpoint%/daemon.sock}
    victim "$home" rm -rf -- "$directory" || fail "$kind victim could not remove stale relocated directory"
    start_daemon "$home" "/tmp/$kind-stale.log"
    pid=$CURRENT_PID
    endpoint_stale=''
    attempt=0
    while ((attempt < 100)); do
      if [[ -r $locator ]]; then
        located=$(<"$locator")
        candidate="/tmp/$located/daemon.sock"
        if [[ $located != "${directory##*/}" && -S $candidate ]]; then
          endpoint_stale=$(doctor_endpoint "$home") || fail "$kind doctor did not report an endpoint after stale recovery"
          break
        fi
      fi
      sleep 0.1
      attempt=$((attempt + 1))
    done
    [[ -n $endpoint_stale && $endpoint_stale != "$endpoint" && -S $endpoint_stale ]] ||
      fail "$kind stale locator was not replaced with a new endpoint"
    verify_relocated_endpoint "$kind" "$endpoint_stale" "$predicted_key" "$predicted"
    assert_attacker_unchanged "$kind" "$object" "$before_stat" "$before_listing"
    if [[ $kind == symlink ]]; then
      [[ $(readlink -- "$object") == "$link_target" ]] || fail 'attacker symlink target changed after stale recovery'
      [[ $(stat -c '%u %g %a %F %i' -- /tmp/attacker-target) == "$target_before_stat" ]] ||
        fail 'attacker symlink target metadata changed after stale recovery'
      [[ $(ls -A -- /tmp/attacker-target) == "$target_before_listing" ]] ||
        fail 'attacker symlink target listing changed after stale recovery'
    fi
    if [[ -n $canary_path ]]; then
      [[ $(stat -c '%u %g %a %F %i' -- "$canary_path") == "$canary_before_stat" ]] ||
        fail "$kind attacker canary metadata changed after stale recovery"
    fi
    stop_daemon "$home" "$pid"
    [[ ! -e $endpoint_stale && ! -L $endpoint_stale ]] || fail "$kind recovered socket remains after stop"
    assert_attacker_unchanged "$kind" "$object" "$before_stat" "$before_listing"
    printf 'PASS: %s stale locator is replaced without touching the hostile entry\n' "$kind"
  }

  run_case dir
  run_case file
  run_case symlink
}

if [[ ${1:-} == --inner ]]; then
  inner_main
  exit $?
fi

if [[ $# -ne 1 ]]; then
  printf 'usage: %s <thegn-binary>\n' "$0" >&2
  exit 2
fi
THEGN_BIN=$1

if [[ $(uname -s) != Linux ]]; then
  printf 'SKIP: Linux user namespaces are required\n'
  exit 77
fi
if ! command -v unshare >/dev/null 2>&1; then
  printf 'SKIP: unshare is unavailable\n'
  exit 77
fi
if ! command -v setpriv >/dev/null 2>&1; then
  printf 'SKIP: setpriv is unavailable\n'
  exit 77
fi
if [[ ! -r /etc/subuid || ! -r /etc/subgid ]]; then
  printf 'SKIP: readable /etc/subuid and /etc/subgid mappings are unavailable\n'
  exit 77
fi

capture_dir=$(mktemp -d)
trap 'rm -rf -- "$capture_dir"' EXIT
# Both host paths can be inaccessible to the mapped namespace ids. Keep the
# binary and this script open across unshare; the inner process copies the
# binary through fd 3 and reads this script through fd 4.
exec 3<"$THEGN_BIN"
exec 4<"$0"
if setpriv --pdeathsig KILL -- unshare --map-auto --mount --propagation private --pid --fork --kill-child -- \
  setpriv --reuid=0 --regid=0 --clear-groups -- bash /dev/fd/4 --inner \
  >"$capture_dir/stdout" 2>"$capture_dir/stderr"; then
  cat "$capture_dir/stdout"
  cat "$capture_dir/stderr" >&2
  exit 0
else
  unshare_status=$?
fi

if ! grep -q '^MULTIUID-READY$' "$capture_dir/stdout"; then
  reason=$(tr '\n' ' ' <"$capture_dir/stderr")
  if [[ -z $reason ]]; then
    reason="unshare exited with status $unshare_status before namespace setup completed"
  fi
  printf 'SKIP: user namespaces, subuid mappings, or sticky tmpfs setup unavailable: %s\n' "$reason"
  exit 77
fi
cat "$capture_dir/stdout"
cat "$capture_dir/stderr" >&2
exit "$unshare_status"
