#!/usr/bin/env bash
# Remove the self-hosted runners set up by ageless-runner-setup.sh: stop +
# uninstall each systemd service, deregister from GitHub, and drop the runner
# dirs. Leaves the warm caches (nix store, sccache, cargo) and host nix in place
# unless you pass PURGE_CACHES=1. Run on ageless-studio as the runner user.
#
#   GH_PAT=<admin PAT> bash scripts/ci/ageless-runner-uninstall.sh
set -euo pipefail

GH_REPO="${GH_REPO:-blakeashleyjr/superzej}"
RUNNER_USER="${RUNNER_USER:-targe}"
RUNNER_COUNT="${RUNNER_COUNT:-4}"
RUNNER_BASE="${RUNNER_BASE:-/home/$RUNNER_USER/actions-runners}"
CACHE_BASE="${CACHE_BASE:-/home/$RUNNER_USER/gha-cache}"
PURGE_CACHES="${PURGE_CACHES:-0}"
GH_PAT="${GH_PAT:-}"

log() { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }

remove_token() {
  if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
    gh api -X POST "repos/$GH_REPO/actions/runners/remove-token" --jq .token
  elif [ -n "$GH_PAT" ]; then
    curl -fsSL -X POST -H "Authorization: Bearer $GH_PAT" \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/$GH_REPO/actions/runners/remove-token" | jq -r .token
  fi
}

for i in $(seq 1 "$RUNNER_COUNT"); do
  dir="$RUNNER_BASE/ageless-$i"
  [ -d "$dir" ] || continue
  log "removing runner ageless-$i…"
  svc_name="$(cat "$dir/.service" 2>/dev/null || true)"
  (cd "$dir" && sudo ./svc.sh stop 2>/dev/null || true)
  (cd "$dir" && sudo ./svc.sh uninstall 2>/dev/null || true)
  [ -n "$svc_name" ] && sudo rm -rf "/etc/systemd/system/${svc_name}.d"
  tok="$(remove_token || true)"
  if [ -n "${tok:-}" ] && [ -f "$dir/config.sh" ]; then
    (cd "$dir" && ./config.sh remove --token "$tok" 2>/dev/null || true)
  fi
  rm -rf "$dir"
done
sudo systemctl daemon-reload

if [ "$PURGE_CACHES" = "1" ]; then
  log "purging warm caches at $CACHE_BASE (PURGE_CACHES=1)…"
  rm -rf "$CACHE_BASE"
fi
log "done."
