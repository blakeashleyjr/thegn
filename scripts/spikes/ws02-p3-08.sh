#!/usr/bin/env bash
# WS02-P3-08 — capture real container-runtime facts (read-only).
# Usage: ws02-p3-08.sh OUTDIR [--events]
#   --events  also run the ONE mutating step: create+remove a labelled scratch
#             container while `docker events` records (needs a reachable engine).
# Captures land in OUTDIR/runtimes/<product>/<version>/ ; evidence index in
# OUTDIR/WS02-P3-08.evidence.txt. Nothing is written outside OUTDIR.
# Procedure: docs/superpowers/specs/sandbox-remote-spikes/WS02-P3-08.md
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS02-P3-08
with_events=0
[ "${2:-}" = "--events" ] && with_events=1
sp_init "$item" "${1:-}"
tmo="${SPIKE_CMD_TIMEOUT:-20}"
captures="$SP_OUT/runtimes"
mkdir -p "$captures"

sp_have docker || sp_have podman || sp_have nerdctl || sp_have finch ||
  sp_die "no container runtime CLI found (docker, podman, nerdctl, finch)"

# Per-product capture dir: runtimes/<product>/<version>/
pdir() { mkdir -p "$captures/$1/$2" && printf '%s' "$captures/$1/$2"; }

# cap PRODUCT_DIR NAME cmd... — one timed, redacted capture file per command.
cap() {
  local dir="$1" name="$2" rc=0
  shift 2
  {
    printf '$ %s\n' "$*"
    sp_timeout "$tmo" "$@" 2>&1 </dev/null || rc=$?
    printf '\n[rc=%s]\n' "$rc"
  } | sp_redact >"$dir/$name.txt"
}

sock_listing() {
  local dir="$1" p
  : >"$dir/sockets.txt"
  for p in /var/run/docker.sock "${XDG_RUNTIME_DIR:-/nonexistent}/docker.sock" \
    "$HOME/.docker/desktop/docker.sock" "$HOME/.docker/run/docker.sock" \
    "$HOME/.colima/default/docker.sock" "$HOME/.orbstack/run/docker.sock" \
    "$HOME/.rd/docker.sock" "$HOME/.lima/docker/sock/docker.sock"; do
    # shellcheck disable=SC2012 # the long listing (mode, owner, type) is the evidence
    ls -l "$p" 2>&1 | sp_redact >>"$dir/sockets.txt" || true
  done
}

product_of_context() { # context name -> product dir name
  case "$1" in
  desktop-linux | desktop-windows) echo docker-desktop ;;
  colima*) echo colima ;;
  orbstack*) echo orbstack ;;
  rancher-desktop*) echo rancher-desktop ;;
  rootless*) echo docker-rootless ;;
  lima*) echo lima-docker ;;
  default) echo docker-engine ;;
  *) echo "docker-context-$1" ;;
  esac
}

products_done=""
if sp_have docker; then
  ctxs="$(docker context ls --format '{{.Name}}' 2>/dev/null || true)"
  [ -n "$ctxs" ] || ctxs=default
  for c in $ctxs; do
    prod="$(product_of_context "$c")"
    ver="$(sp_timeout "$tmo" docker --context "$c" version --format '{{.Server.Version}}' 2>/dev/null </dev/null || true)"
    [ -n "$ver" ] || ver="unreachable-$(docker --version 2>/dev/null | awk '{print $3}' | tr -d ,)"
    d="$(pdir "$prod" "$ver")"
    cap "$d" docker-version docker --context "$c" version
    cap "$d" docker-info docker --context "$c" info --format '{{json .}}'
    cap "$d" docker-context-ls docker context ls --format json
    cap "$d" docker-context-inspect docker context inspect "$c" --format json
    sock_listing "$d"
    products_done="$products_done $prod/$ver"
    if [ "$with_events" = 1 ]; then
      case "$ver" in
      unreachable-*) ;;
      *)
        # The one mutating step: a labelled scratch container, removed by trap.
        lbl="thegn.spike=ws02-p3-08-$$"
        img="${SPIKE_IMAGE:-alpine:3}"
        sp_say "events: context $c creates and removes a container labelled $lbl"
        sp_on_exit "docker --context '$c' ps -aq --filter 'label=$lbl' | xargs -r docker --context '$c' rm -f"
        (sp_timeout 30 docker --context "$c" events --since 1s --until 12s --format '{{json .}}' \
          >"$d/docker-events.txt" 2>&1) &
        ev=$!
        sleep 2
        docker --context "$c" create --label "$lbl" --name "tgspike-$$" "$img" true >/dev/null 2>&1 || true
        docker --context "$c" rm -f "tgspike-$$" >/dev/null 2>&1 || true
        wait "$ev" 2>/dev/null || true
        sp_redact <"$d/docker-events.txt" >"$d/docker-events.r" && mv "$d/docker-events.r" "$d/docker-events.txt"
        ;;
      esac
    fi
  done
fi

if sp_have podman; then
  ver="$(podman --version 2>/dev/null | awk '{print $3}')"
  rootless="$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null || echo unknown)"
  prod=podman-rootful
  [ "$rootless" = true ] && prod=podman-rootless
  d="$(pdir "$prod" "${ver:-unknown}")"
  cap "$d" podman-version podman version
  cap "$d" podman-info podman info --format json
  cap "$d" machine-list podman machine list --format json
  cap "$d" machine-inspect podman machine inspect --format json
  sock_listing "$d"
  products_done="$products_done $prod/${ver:-unknown}"
fi

if sp_have nerdctl; then
  ver="$(nerdctl --version 2>/dev/null | awk '{print $3}')"
  d="$(pdir nerdctl "${ver:-unknown}")"
  cap "$d" nerdctl-version nerdctl version
  cap "$d" nerdctl-info nerdctl info
  products_done="$products_done nerdctl/${ver:-unknown}"
fi

if sp_have finch; then
  ver="$(finch --version 2>/dev/null | awk '{print $NF}')"
  d="$(pdir finch "${ver:-unknown}")"
  cap "$d" finch-vm-status finch vm status
  products_done="$products_done finch/${ver:-unknown}"
fi

sp_note ""
sp_note "## captures"
for p in $products_done; do sp_note "captured: $p"; done
sp_note "events_step: $with_events"
sp_note "NEXT: review every file for tokens, registry auth, private hostnames before committing (docker info / context inspect can contain them)."
sp_verdict "a product preset is verified only with a capture from that product version or newer; absent products stay 'candidate only (no capture)'" \
  "captured:${products_done:- none}"
