# shellcheck shell=bash
# Expose the dev-shell Podman socket to Docker-compatible clients.
#
# The shim exports DOCKER_HOST and may create ~/.docker/docker.sock only when
# that leaf is absent. It never replaces or removes existing Docker state. The
# optional compatibility link is persistent; remove it manually with
# `rm ~/.docker/docker.sock` if you no longer want it (only if it still points
# at this Podman socket).
#
# Sourced by both dev shells through flake.nix. Quiet no-op when Podman is
# unavailable, when a caller selected DOCKER_HOST, or when HOME is read-only.
_pod="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/podman/podman.sock"
if [ -z "${DOCKER_HOST:-}" ] && command -v podman >/dev/null 2>&1 && [ -S "$_pod" ]; then
  export DOCKER_HOST="unix://$_pod"
  _home_docker="${HOME:-}/.docker"

  if [ -z "${HOME:-}" ] || [ ! -d "$HOME" ]; then
    :
  elif [ -L "$_home_docker" ]; then
    printf '%s\n' "dev-docker-shim: refusing symlinked $_home_docker; remove the symlink or configure DOCKER_HOST" >&2
  elif [ -e "$_home_docker" ] && [ ! -d "$_home_docker" ]; then
    printf '%s\n' "dev-docker-shim: $_home_docker is not a directory; configure DOCKER_HOST or move it aside" >&2
  elif [ -d "$_home_docker" ]; then
    if [ -L "$_home_docker/docker.sock" ]; then
      if [ "$(readlink "$_home_docker/docker.sock" 2>/dev/null)" != "$_pod" ]; then
        printf '%s\n' "dev-docker-shim: preserving existing $_home_docker/docker.sock; configure DOCKER_HOST or move it aside" >&2
      fi
    elif [ -e "$_home_docker/docker.sock" ]; then
      printf '%s\n' "dev-docker-shim: preserving existing $_home_docker/docker.sock; configure DOCKER_HOST or move it aside" >&2
    elif [ -w "$_home_docker" ]; then
      if ! ln -s "$_pod" "$_home_docker/docker.sock" 2>/dev/null; then
        # Another shell may have created the same link between the absence
        # check and ln. Treat that as an idempotent result; never replace it.
        if [ ! -L "$_home_docker/docker.sock" ] || [ "$(readlink "$_home_docker/docker.sock" 2>/dev/null)" != "$_pod" ]; then
          printf '%s\n' "dev-docker-shim: could not create $_home_docker/docker.sock; check directory permissions and existing Docker configuration" >&2
        fi
      fi
    fi
  elif [ -w "$HOME" ]; then
    if mkdir "$_home_docker" 2>/dev/null; then
      if ! ln -s "$_pod" "$_home_docker/docker.sock" 2>/dev/null; then
        printf '%s\n' "dev-docker-shim: could not create $_home_docker/docker.sock; check directory permissions and existing Docker configuration" >&2
      fi
    elif [ -L "$_home_docker" ]; then
      printf '%s\n' "dev-docker-shim: refusing symlinked $_home_docker; remove the symlink or configure DOCKER_HOST" >&2
    elif [ ! -d "$_home_docker" ]; then
      printf '%s\n' "dev-docker-shim: could not create $_home_docker; check directory permissions" >&2
    fi
  fi
  unset _home_docker
fi
unset _pod
