#!/usr/bin/env bash
set -euo pipefail

shim=${1:?usage: dev-docker-shim-test.sh SHIM}
tmp=$(mktemp -d)
trap 'chmod -R u+w "$tmp" 2>/dev/null || true; rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/runtime/podman"
cat >"$tmp/bin/podman" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$tmp/bin/podman"
runtime=$tmp/runtime
if ! python3 -c 'import socket, sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.close()' "$runtime/podman/podman.sock" 2>/dev/null; then
  # Some restricted runners prohibit creating AF_UNIX sockets. Reuse the local
  # Podman socket when present; otherwise report that the environment skipped
  # the behavioral checks instead of failing for sandbox policy.
  if [ -S /run/podman/podman.sock ]; then
    runtime=/run
  else
    echo 'dev-docker-shim checks skipped: runner cannot create a Unix socket'
    exit 1
  fi
fi

run_shim() {
  local home=$1
  shift
  # shellcheck disable=SC2016
  env -u DOCKER_HOST PATH="$tmp/bin:$PATH" XDG_RUNTIME_DIR="$runtime" HOME="$home" bash -c 'source "$1"' _ "$shim"
}

# A regular-file collision stays byte-for-byte intact and reports a conflict.
mkdir -p "$tmp/regular/.docker"
printf 'user data\n' >"$tmp/regular/.docker/docker.sock"
if output=$(run_shim "$tmp/regular" 2>&1); then :; fi
test "$(cat "$tmp/regular/.docker/docker.sock")" = 'user data'
[[ $output == *"preserving existing"* ]]

# A real socket leaf is preserved just like any other pre-existing entry.
mkdir -p "$tmp/socket-home/.docker"
if python3 -c 'import socket, sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.close()' "$tmp/socket-home/.docker/docker.sock" 2>/dev/null; then
  output=$(run_shim "$tmp/socket-home" 2>&1)
  test -S "$tmp/socket-home/.docker/docker.sock"
  [[ $output == *"preserving existing"* ]]
fi

# A foreign symlink is neither replaced nor followed for mutation.
mkdir -p "$tmp/foreign/.docker"
ln -s "$tmp/target" "$tmp/foreign/.docker/docker.sock"
if output=$(run_shim "$tmp/foreign" 2>&1); then :; fi
test "$(readlink "$tmp/foreign/.docker/docker.sock")" = "$tmp/target"
[[ $output == *"preserving existing"* ]]

# A symlinked .docker parent is refused and its target remains untouched.
mkdir -p "$tmp/parent/.docker-target"
ln -s "$tmp/parent/.docker-target" "$tmp/parent/.docker"
if output=$(run_shim "$tmp/parent" 2>&1); then :; fi
[[ $output == *"refusing symlinked"* ]]
test ! -e "$tmp/parent/.docker-target/docker.sock"

# An explicit DOCKER_HOST causes no persistent Docker-path mutation.
mkdir -p "$tmp/explicit"
# shellcheck disable=SC2016
if output=$(env PATH="$tmp/bin:$PATH" XDG_RUNTIME_DIR="$runtime" HOME="$tmp/explicit" DOCKER_HOST=unix:///chosen/socket bash -c 'source "$1"' _ "$shim" 2>&1); then :; fi
test ! -e "$tmp/explicit/.docker"
test ! -L "$tmp/explicit/.docker"
test -z "$output"

# An absent leaf is created once; a second source is silent and idempotent.
mkdir -p "$tmp/idempotent/.docker"
output=$(run_shim "$tmp/idempotent" 2>&1)
test -L "$tmp/idempotent/.docker/docker.sock"
test "$(readlink "$tmp/idempotent/.docker/docker.sock")" = "$runtime/podman/podman.sock"
output=$(run_shim "$tmp/idempotent" 2>&1)
test -z "$output"

# On an actually non-writable home, shell entry stays quiet and creates nothing.
mkdir -p "$tmp/readonly"
chmod 555 "$tmp/readonly"
if [ ! -w "$tmp/readonly" ]; then
  output=$(run_shim "$tmp/readonly" 2>&1)
  test -z "$output"
  test ! -e "$tmp/readonly/.docker"
fi

echo 'dev-docker-shim checks passed'
