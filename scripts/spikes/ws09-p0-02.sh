#!/usr/bin/env bash
# WS09-P0-02 — connector-recipe facts reachable on Linux without paid accounts.
# Usage: ws09-p0-02.sh OUTDIR
# Optional, off by default (each one costs time, network or creates something):
#   RUN_KIND=1     create a kind cluster 'tgspike-<pid>', 50 MB kubectl exec -i sha256
#                  round trip, then delete the cluster (trap)
#   RUN_IMAGES=1   pull debian:12 ubuntu:24.04 alpine:3.20 fedora:41 with docker or
#                  podman and test for zstd/gzip
#   GIT_BINS=/path/git-2.30:/path/git-2.54   colon-separated git binaries for the
#                  bundle matrix (default: the git on PATH)
# Writes OUTDIR/WS09-P0-02.evidence.txt including draft table rows for
# config/recipes/connector/VERIFICATION.md. A tool that is absent becomes a
# "still unverified" row with the exact command; nothing is guessed.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS09-P0-02
sp_init "$item" "${1:-}"
today="$(date -u +%F)"
rows="$SP_OUT/$item/rows.md"
printf '| recipe or topic | command run | tool version | date | result | evidence level | decision |\n|---|---|---|---|---|---|---|\n' >"$rows"
# shellcheck disable=SC2016 # backticks are markdown, not expansions
row() { printf '| %s | `%s` | %s | %s | %s | %s | %s |\n' "$1" "$2" "$3" "$today" "$4" "$5" "$6" >>"$rows"; }

# probe LABEL TOOL ARGS... : capture a command; absent tool => still-unverified row.
probe() {
  local label="$1" tool="$2" ver
  shift 2
  if ! sp_have "$tool"; then
    row "$label" "$tool $*" "absent" "not run" "still unverified" "install $tool and run: $tool $*"
    return 0
  fi
  ver="$(sp_timeout 10 "$tool" --version 2>&1 </dev/null | head -n1 | sp_redact || true)"
  sp_cap "$(printf '%s' "$label" | tr -c 'A-Za-z0-9' '_')" 20 "$tool" "$@"
  row "$label" "$tool $*" "${ver:-unknown}" "rc=$SP_RC (see evidence capture)" "verified on this host" "TODO: decide template/flag from the capture"
}

probe "tailscale status (exit codes)" tailscale status
probe "tailscale login --login-server" tailscale login --help
probe "tailscale nc" tailscale nc --help
probe "cloudflared access ssh" cloudflared access ssh --help
probe "cloudflared access login" cloudflared access login --help
probe "limactl shell" limactl shell --help
probe "incus exec" incus exec --help
probe "multipass exec" multipass exec --help
probe "dumbpipe connect-tcp" dumbpipe connect-tcp --help
probe "wstunnel client (stdio form)" wstunnel client --help
probe "netbird status --json" netbird status --json
probe "zerotier-cli" zerotier-cli info
probe "nebula" nebula -help
probe "zrok access private" zrok access private --help
probe "step ssh" step ssh --help
probe "opkssh" opkssh --help
probe "et" et --help
probe "shpool attach -f" shpool attach --help
probe "tmux new-session -A -s" tmux -V

# dumbpipe: is the ticket accepted through env or stdin? (help text only; the source
# read is a manual step in the procedure page)
if sp_have dumbpipe; then
  dp="$SP_OUT/$item/dumbpipe_connect_tcp.out"
  if grep -qiE 'env|stdin' "$dp" 2>/dev/null; then
    sp_kv dumbpipe_ticket_env_or_stdin_in_help "MENTIONED (read the lines; confirm in source before deciding)"
  else
    sp_kv dumbpipe_ticket_env_or_stdin_in_help "not mentioned in --help (still read src/main.rs: help text is not proof)"
  fi
fi

# git bundle matrix
IFS=: read -r -a gits <<<"${GIT_BINS:-$(command -v git)}"
for g in "${gits[@]}"; do
  [ -x "$g" ] || {
    row "git bundle" "$g" "absent" "not run" "still unverified" "path not executable"
    continue
  }
  t="$(mktemp -d)"
  sp_on_exit "rm -rf '$t'"
  gv="$("$g" --version | awk '{print $3}')"
  (
    cd "$t"
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_AUTHOR_NAME=s GIT_AUTHOR_EMAIL=s@x GIT_COMMITTER_NAME=s GIT_COMMITTER_EMAIL=s@x
    "$g" init -q -b main src
    cd src
    echo one >f && "$g" add f && "$g" -c commit.gpgsign=false commit -qm one
    tip1="$("$g" rev-parse HEAD)"
    echo two >f && "$g" -c commit.gpgsign=false commit -qam two
    "$g" bundle create - refs/heads/main >"$t/full.bundle" 2>/dev/null
    "$g" bundle create - refs/heads/main "^$tip1" >"$t/inc.bundle" 2>/dev/null
    tip2="$("$g" rev-parse HEAD)"
    cd "$t"
    "$g" init -q -b main full && "$g" -C full fetch -q "$t/full.bundle" main:refs/heads/main
    "$g" init -q -b main inc && "$g" -C inc fetch -q "$t/src" "$tip1:refs/heads/base" # base objects
    "$g" -C inc fetch -q "$t/inc.bundle" main:refs/heads/main
    [ "$("$g" -C full rev-parse main)" = "$tip2" ] && [ "$("$g" -C inc rev-parse main)" = "$tip2" ]
  ) >"$SP_OUT/$item/bundle-$gv.log" 2>&1 && res="full and incremental bundle to stdout then fetch: OK" || res="FAILED (see bundle-$gv.log)"
  row "git bundle create - (full, ^tip)" "$g bundle create - refs/heads/main [^tip]" "$gv" "$res" "verified on this host" "TODO"
done

# kind + kubectl exec -i binary safety (opt-in)
if [ "${RUN_KIND:-0}" = 1 ]; then
  sp_need kind kubectl sha256sum head
  cname="tgspike-$$"
  sp_on_exit "kind delete cluster --name '$cname'"
  kind create cluster --name "$cname" >"$SP_OUT/$item/kind-create.log" 2>&1
  big="$(mktemp)"
  sp_on_exit "rm -f '$big'"
  head -c 52428800 /dev/urandom >"$big"
  sent="$(sha256sum <"$big" | awk '{print $1}')"
  kubectl --context "kind-$cname" run tgcat --image=busybox --restart=Never -- sleep 900 >/dev/null
  kubectl --context "kind-$cname" wait --for=condition=Ready pod/tgcat --timeout=180s >/dev/null
  got="$(kubectl --context "kind-$cname" exec -i tgcat -- cat <"$big" | sha256sum | awk '{print $1}')"
  if [ "$sent" = "$got" ]; then res="50 MB sha256 match"; else res="MISMATCH sent=$sent got=$got"; fi
  row "kubectl exec -i binary safety" "kubectl exec -i tgcat -- cat < 50MB" "$(kubectl version --client 2>/dev/null | head -n1)" "$res" "verified on this host" "TODO"
else
  row "kubectl exec -i binary safety" "kind create cluster; kubectl exec -i POD -- cat < 50MB" "not run" "RUN_KIND not set" "still unverified" "re-run with RUN_KIND=1"
fi

# zstd / gzip presence in base images (opt-in)
if [ "${RUN_IMAGES:-0}" = 1 ]; then
  eng=""
  sp_have docker && eng=docker
  [ -n "$eng" ] || { sp_have podman && eng=podman; }
  [ -n "$eng" ] || sp_die "RUN_IMAGES=1 needs docker or podman"
  for img in debian:12 ubuntu:24.04 alpine:3.20 fedora:41; do
    # shellcheck disable=SC2016 # expanded inside the container
    out="$(sp_timeout 300 "$eng" run --rm "$img" sh -c 'printf "zstd=%s gzip=%s" "$(command -v zstd || echo no)" "$(command -v gzip || echo no)"' 2>&1 </dev/null || true)"
    row "base image decoders" "$eng run --rm $img sh -c 'command -v zstd gzip'" "$img" "$out" "verified on this host" "decoder table input"
  done
else
  row "base image decoders" "docker run --rm <img> sh -c 'command -v zstd gzip'" "not run" "RUN_IMAGES not set" "still unverified" "re-run with RUN_IMAGES=1 (macOS 15: check by hand)"
fi
row "systemd-ssh-proxy over vsock" "ssh vsock/<cid> (local VM)" "not run" "manual, shared with WS07" "still unverified" "see procedure page"

sp_note ""
sp_note "## draft VERIFICATION.md rows (edit decisions, then append to config/recipes/connector/VERIFICATION.md)"
cat "$rows" >>"$SP_EVID"
sp_verdict "each locally reachable row ends with a decided template or an explicit documented-only tier; dumbpipe: env/stdin accepted => WS09-P7-03 moves the ticket, else documents the residual" \
  "draft rows written ($(grep -c '^| ' "$rows") lines incl. header); decisions are a human step"
