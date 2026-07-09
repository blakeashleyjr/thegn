#!/usr/bin/env bash
# Set up N self-hosted GitHub Actions runners on ageless-studio, each pinned to
# a CPU budget, sharing warm host-level caches (nix store + sccache + cargo
# registry) so CI builds are near-instant after the first warm run.
#
# Run this ON ageless-studio, as the runner user (default: targe):
#
#   NIX_GITHUB_TOKEN=<classic PAT, repo scope> \
#   GH_PAT=<PAT with repo admin, to auto-register runners> \
#     bash scripts/ci/ageless-runner-setup.sh
#
# It is IDEMPOTENT — re-run it to add nix, re-register runners, or bump config.
# Teardown with scripts/ci/ageless-runner-uninstall.sh.
#
# Why self-hosted + warm caches (see docs/self-hosted-ci.md):
#   * /nix/store persists → `nix build .#default` / `nix develop` are cache hits
#     after the first build (the ~60-min cold nix-build on GitHub is gone).
#   * a shared sccache dir persists → warm Rust crate compiles across every job
#     and every runner.
#   * CARGO_HOME (crate registry) persists → no re-download.
#   * each runner's _work/ persists → incremental `target/` between jobs.
set -euo pipefail

# ── knobs (override via env) ─────────────────────────────────────────────────
GH_REPO="${GH_REPO:-blakeashleyjr/superzej}"
RUNNER_USER="${RUNNER_USER:-targe}"
RUNNER_COUNT="${RUNNER_COUNT:-4}"
RUNNER_CPUS="${RUNNER_CPUS:-3}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,linux,x64,ageless,nix}"
RUNNER_BASE="${RUNNER_BASE:-/home/$RUNNER_USER/actions-runners}"
CACHE_BASE="${CACHE_BASE:-/home/$RUNNER_USER/gha-cache}"
SCCACHE_SIZE="${SCCACHE_SIZE:-60G}"
NIX_GITHUB_TOKEN="${NIX_GITHUB_TOKEN:-}" # PAT for the workflow's private flake inputs
GH_PAT="${GH_PAT:-}"                     # PAT (repo admin) used to mint runner registration tokens

log() { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
die() {
  printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2
  exit 1
}

# ── preflight ────────────────────────────────────────────────────────────────
[ "$(whoami)" = "$RUNNER_USER" ] || die "run this as '$RUNNER_USER' (you are '$(whoami)')"
command -v systemctl >/dev/null || die "systemd required (WSL: set 'systemd=true' in /etc/wsl.conf, then 'wsl --shutdown')"
command -v sudo >/dev/null || die "sudo required (runner services + CPU limits are system units)"
for t in curl tar jq; do command -v "$t" >/dev/null || die "missing '$t'"; done
HAVE_CORES="$(nproc)"
NEED_CORES=$((RUNNER_COUNT * RUNNER_CPUS))
[ "$HAVE_CORES" -ge "$NEED_CORES" ] ||
  log "WARNING: $RUNNER_COUNT×${RUNNER_CPUS}cpu = $NEED_CORES cores requested but box has $HAVE_CORES; CPUQuota is a limit (not a reservation), so this still works but runners will contend."

# a registration token minter: prefer authed `gh`, else the REST API with GH_PAT.
reg_token() {
  if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
    gh api -X POST "repos/$GH_REPO/actions/runners/registration-token" --jq .token
  elif [ -n "$GH_PAT" ]; then
    curl -fsSL -X POST \
      -H "Authorization: Bearer $GH_PAT" \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/$GH_REPO/actions/runners/registration-token" | jq -r .token
  else
    die "need a runner registration token: either 'gh auth login' as a repo admin, or pass GH_PAT=<admin PAT>"
  fi
}

# ── 1. host nix (persistent /nix/store is the single biggest cache win) ───────
if ! command -v nix >/dev/null && [ ! -e /nix/var/nix/profiles/default/bin/nix ]; then
  log "installing nix (Determinate Systems, multi-user)…"
  curl -fsSL https://install.determinate.systems/nix |
    sh -s -- install --no-confirm
else
  log "nix already installed — leaving it in place"
fi
# a good host nix.conf: flakes on, sane parallelism, KEEP build outputs so the
# dev-shell closure survives GC, and access-tokens so private flake inputs
# resolve WITHOUT the workflow needing a secret.
NIX_CONF_DIR="/home/$RUNNER_USER/.config/nix"
mkdir -p "$NIX_CONF_DIR"
{
  echo "experimental-features = nix-command flakes"
  echo "max-jobs = auto"
  echo "cores = $RUNNER_CPUS"
  echo "keep-outputs = true"
  echo "keep-derivations = true"
  echo "connect-timeout = 20"
  echo "download-attempts = 5"
  [ -n "$NIX_GITHUB_TOKEN" ] && echo "access-tokens = github.com=$NIX_GITHUB_TOKEN"
} >"$NIX_CONF_DIR/nix.conf"
chmod 600 "$NIX_CONF_DIR/nix.conf"
log "wrote $NIX_CONF_DIR/nix.conf"

# ── 2. warm shared caches (host-level, shared across all runners) ─────────────
mkdir -p "$CACHE_BASE/cargo" "$CACHE_BASE/sccache"
# The runner reads a `.env` (job env) and `.path` (job PATH) file from each
# runner dir — the official customization hook. One shared copy, linked in.
cat >"$CACHE_BASE/runner.env" <<EOF
# Warm-cache env injected into every CI job on this host (see ageless-runner-setup.sh).
CARGO_HOME=$CACHE_BASE/cargo
RUSTC_WRAPPER=sccache
SCCACHE_DIR=$CACHE_BASE/sccache
SCCACHE_CACHE_SIZE=$SCCACHE_SIZE
CARGO_INCREMENTAL=0
DO_NOT_TRACK=1
OPENSPEC_TELEMETRY=0
EOF
# PATH for jobs: nix profiles first, then NixOS + Debian/WSL system dirs (extra
# entries that don't exist are harmless), then the shared cargo bin.
cat >"$CACHE_BASE/runner.path" <<EOF
/home/$RUNNER_USER/.nix-profile/bin
/nix/var/nix/profiles/default/bin
$CACHE_BASE/cargo/bin
/run/current-system/sw/bin
/usr/local/sbin
/usr/local/bin
/usr/sbin
/usr/bin
/sbin
/bin
EOF
log "wrote warm-cache env + path under $CACHE_BASE"

# ── 3. download the actions runner once, fan out to runner-1..N ──────────────
mkdir -p "$RUNNER_BASE"
RUNNER_VER="$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | jq -r .tag_name | sed 's/^v//')"
TARBALL="actions-runner-linux-x64-${RUNNER_VER}.tar.gz"
CACHE_TARBALL="$RUNNER_BASE/$TARBALL"
[ -f "$CACHE_TARBALL" ] || {
  log "downloading actions-runner v$RUNNER_VER…"
  curl -fsSL -o "$CACHE_TARBALL" \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VER}/${TARBALL}"
}

for i in $(seq 1 "$RUNNER_COUNT"); do
  name="ageless-$i"
  dir="$RUNNER_BASE/$name"
  log "── runner $i/$RUNNER_COUNT: $name ──"
  mkdir -p "$dir"
  [ -f "$dir/config.sh" ] || tar xzf "$CACHE_TARBALL" -C "$dir"

  # The runner is a .NET app; on a fresh distro it needs libicu/libssl etc.
  # `installdependencies.sh` installs them system-wide — do it once.
  if [ ! -f "$RUNNER_BASE/.deps-installed" ]; then
    log "  installing runner system deps (libicu/.NET)…"
    (cd "$dir" && sudo ./bin/installdependencies.sh) && touch "$RUNNER_BASE/.deps-installed"
  fi

  # warm-cache hooks (per-runner copies of the shared env/path)
  cp "$CACHE_BASE/runner.env" "$dir/.env"
  cp "$CACHE_BASE/runner.path" "$dir/.path"

  # (re)register. --replace lets us re-run this script to refresh config.
  if [ -f "$dir/.runner" ]; then
    log "  already registered — refreshing service only"
  else
    log "  registering with $GH_REPO…"
    (cd "$dir" && ./config.sh \
      --url "https://github.com/$GH_REPO" \
      --token "$(reg_token)" \
      --name "$name" \
      --labels "$RUNNER_LABELS" \
      --work _work \
      --unattended --replace)
  fi

  # install + start as a systemd service (svc.sh handles the unit + linger)
  (cd "$dir" && sudo ./svc.sh install "$RUNNER_USER" && sudo ./svc.sh start)

  # CPU cap: a drop-in on the runner's service. CPUQuota=<cpus*100>% limits the
  # runner (and every job it spawns) to that many cores' worth of CPU time.
  svc_name="$(cat "$dir/.service" 2>/dev/null || true)"
  [ -n "$svc_name" ] || die "  could not determine systemd service name for $name"
  dropin="/etc/systemd/system/${svc_name}.d"
  sudo mkdir -p "$dropin"
  sudo tee "$dropin/10-superzej-ci.conf" >/dev/null <<EOF
[Service]
# Pin this runner to ${RUNNER_CPUS} cores' worth of CPU (4×${RUNNER_CPUS} across the box).
CPUQuota=$((RUNNER_CPUS * 100))%
CPUWeight=100
# keep a wedged job from OOM-killing the host
MemoryHigh=12G
EOF
  sudo systemctl daemon-reload
  sudo systemctl restart "$svc_name"
  log "  $svc_name up (CPUQuota=$((RUNNER_CPUS * 100))%)"
done

log "done. Verify on GitHub: Settings → Actions → Runners (expect $RUNNER_COUNT × 'ageless-*' idle)."
log "Or: gh api repos/$GH_REPO/actions/runners --jq '.runners[]|{name,status}'"
