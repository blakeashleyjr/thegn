# shellcheck shell=bash
# Make podman transparently docker-compatible for the dev shell, quietly.
#
# thegn dev happens inside sandboxes (bwrap; the AI agent's own bwrap) that bind
# /home read-only. An external podman-docker compat step keeps re-creating
# ~/.docker/docker.sock via an atomic `ln -s <tmp> && mv`, which fails loudly
# under that read-only bind ("ln: … Read-only file system") and litters
# ~/.docker with stale temp symlinks. This shim replaces that dependency: it
# exports DOCKER_HOST (so nothing needs the symlink at all) and only touches the
# symlink where ~/.docker is writable — so it is silent under the read-only bind.
#
# Sourced by the flake `devShellHook` (flake.nix), which both the host `default`
# and the sandbox/sprite `sprite-full` shells use — so they share one impl.
# Idempotent; a no-op when podman/its socket are absent.
_pod="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/podman/podman.sock"
if command -v podman >/dev/null 2>&1 && [ -S "$_pod" ]; then
  # docker CLIs / testcontainers / act / `just act` read DOCKER_HOST; setting it
  # means no ~/.docker/docker.sock symlink is required at all (docs/local-ci.md).
  [ -z "${DOCKER_HOST:-}" ] && export DOCKER_HOST="unix://$_pod"
  _dkr="$HOME/.docker"
  # Self-heal the compat symlink ONLY where writable (skips silently under the
  # read-only /home sandbox bind — no error output).
  if { [ -d "$_dkr" ] && [ -w "$_dkr" ]; } || { [ ! -e "$_dkr" ] && mkdir -p "$_dkr" 2>/dev/null; }; then
    if [ "$(readlink "$_dkr/docker.sock" 2>/dev/null)" != "$_pod" ]; then
      ln -sfn "$_pod" "$_dkr/docker.sock" 2>/dev/null || true
    fi
    # Sweep stale temp symlinks left by a failed external atomic swap: only
    # those that point exactly at the podman socket (clearly leftover), never
    # docker.sock or unrelated files.
    for _l in "$_dkr"/*; do
      [ -L "$_l" ] || continue
      [ "${_l##*/}" = "docker.sock" ] && continue
      if [ "$(readlink "$_l" 2>/dev/null)" = "$_pod" ]; then
        rm -f "$_l" 2>/dev/null || true
      fi
    done
    unset _l
  fi
  unset _dkr
fi
unset _pod
