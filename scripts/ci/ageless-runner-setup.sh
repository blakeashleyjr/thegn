#!/usr/bin/env bash
# Configure root-owned JIT runner loops. Run this script as an operator; jobs
# execute as the isolated, non-login thegn-ci account.
set -euo pipefail

GH_REPO="${GH_REPO:-blakeashleyjr/thegn}"
RUNNER_USER="${RUNNER_USER:-thegn-ci}"
RUNNER_COUNT="${RUNNER_COUNT:-4}"
RUNNER_CPUS="${RUNNER_CPUS:-3}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,linux,x64,ageless,nix}"
RUNNER_BASE="${RUNNER_BASE:-/var/lib/thegn-ci/runners}"
RUNNER_TOKEN_FILE="${RUNNER_TOKEN_FILE:-/etc/thegn-ci/registration.token}"
RUNNER_LOOP_FILE="${RUNNER_LOOP_FILE:-/usr/local/libexec/thegn-ci/runner-loop}"
RUNNER_UNIT_FILE="${RUNNER_UNIT_FILE:-/etc/systemd/system/thegn-ci-runner@.service}"
RUNNER_ARCHIVE_FILE="${RUNNER_ARCHIVE_FILE:-$RUNNER_BASE/actions-runner-linux-x64.tar.gz}"
NIX_GITHUB_TOKEN="${NIX_GITHUB_TOKEN:-}"
GH_PAT="${GH_PAT:-}"

log() { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
die() {
  printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2
  return 1
}

unexport_credentials() {
  # GH_PAT reaches the privileged install only through stdin; neither credential
  # is inherited by preflight, setup, or runner child processes.
  export -n NIX_GITHUB_TOKEN GH_PAT
}

runner_identity_preflight() {
  local runner_uid operator
  operator="$(id -un)"
  if [ "$RUNNER_USER" = "$operator" ]; then
    printf 'runner account %s is the invoking operator; choose a dedicated account (fix: set RUNNER_USER=thegn-ci)\n' "$RUNNER_USER" >&2
    return 1
  fi
  if ! runner_uid="$(id -u "$RUNNER_USER" 2>/dev/null)"; then
    return 0
  fi
  if [ "$runner_uid" = 0 ] || [ "$RUNNER_USER" = root ]; then
    printf 'runner account %s is root (fix: use a dedicated non-root account such as thegn-ci)\n' "$RUNNER_USER" >&2
    return 1
  fi
  return 0
}

runner_preflight() {
  local failures=0 groups shell home sudo_output account_line path
  unexport_credentials
  runner_identity_preflight || failures=1
  if [ -n "$NIX_GITHUB_TOKEN" ]; then
    printf 'NIX_GITHUB_TOKEN is set; runner jobs must not receive Nix credentials (fix: unset NIX_GITHUB_TOKEN)\n' >&2
    failures=1
  fi
  if ! account_line="$(getent passwd "$RUNNER_USER")"; then
    # The first preflight runs before account creation. Identity and token checks
    # still apply; account-specific checks run immediately after creation.
    if [ "$failures" -ne 0 ]; then return 1; fi
    return 0
  fi
  IFS=: read -r _ _ _ _ _ home shell <<<"$account_line"
  if [[ $shell != */nologin && $shell != */false ]]; then
    printf 'runner account %s has interactive shell %s (fix: set its shell to /usr/sbin/nologin)\n' "$RUNNER_USER" "$shell" >&2
    failures=1
  fi
  if ! groups="$(id -nG "$RUNNER_USER" 2>/dev/null)"; then
    printf 'cannot inspect groups for runner account %s (fix: repair the account and retry)\n' "$RUNNER_USER" >&2
    failures=1
  else
    for group in sudo wheel admin docker podman libvirt kvm lxd; do
      if [[ " $groups " == *" $group "* ]]; then
        printf 'runner account %s belongs to privileged group %s (fix: remove it with usermod -G)\n' "$RUNNER_USER" "$group" >&2
        failures=1
      fi
    done
  fi
  if ! sudo_output="$(sudo -n -l -U "$RUNNER_USER" 2>&1)"; then
    printf 'cannot verify sudo rights for runner account %s (fix: make sudo -n -l -U succeed, then remove any grants)\n' "$RUNNER_USER" >&2
    failures=1
  elif [[ $sudo_output != *"is not allowed to run sudo"* ]]; then
    printf 'runner account %s has sudo rights (fix: remove its sudoers grant)\n' "$RUNNER_USER" >&2
    failures=1
  fi
  for path in "$home/.config/gh/hosts.yml" "$home/.ssh" "$home/.gnupg" \
    "$home/.git-credentials" "$home/.config/nix/nix.conf" "$home/.docker/config.json"; do
    if [ -e "$path" ] || [ -L "$path" ]; then
      if [[ $path == */nix.conf ]] && ! grep -qi 'access-tokens' "$path"; then
        continue
      fi
      printf 'runner HOME contains credentials or persistent auth state at %s (fix: remove it and keep credentials outside the runner account)\n' "$path" >&2
      failures=1
    fi
  done
  if [ "$failures" -ne 0 ]; then
    die 'runner preflight refused an unsafe account; apply each fix above and retry'
    return 1
  fi
  return 0
}

ensure_runner_account() {
  if getent passwd "$RUNNER_USER" >/dev/null; then return 0; fi
  if [ -e /etc/NIXOS ]; then
    die 'NixOS does not persist useradd; add this declaration to configuration.nix and rebuild: users.users.thegn-ci = { isSystemUser = true; group = "thegn-ci"; home = "/var/lib/thegn-ci"; createHome = true; };'
    return 1
  fi
  sudo useradd --system --create-home --home-dir /var/lib/thegn-ci --shell /usr/sbin/nologin "$RUNNER_USER"
}

secure_runner_home_parent() {
  local home
  home="$(getent passwd "$RUNNER_USER" | cut -d: -f6)"
  if sudo test -L "$home"; then
    die "runner HOME is a symlink: $home"
    return 1
  fi
  sudo chown root:root "$home"
  sudo chmod 0755 "$home"
}

prepare_registration_token() {
  local token_dir token_mode token_owner dir_mode dir_owner
  token_dir="$(dirname "$RUNNER_TOKEN_FILE")"
  if sudo test -L "$token_dir"; then
    die "registration token directory is a symlink: $token_dir"
    return 1
  fi
  sudo install -d -m 0700 -o root -g root "$token_dir"
  dir_owner="$(sudo stat -c '%u:%g' "$token_dir")"
  dir_mode="$(sudo stat -c '%a' "$token_dir")"
  if [ "$dir_owner" != '0:0' ] || [ "$dir_mode" != 700 ]; then
    die "registration token directory must be root:root mode 0700: $token_dir"
    return 1
  fi
  if sudo test -e "$RUNNER_TOKEN_FILE" || sudo test -L "$RUNNER_TOKEN_FILE"; then
    if sudo test -L "$RUNNER_TOKEN_FILE"; then
      die "registration token path is a symlink: $RUNNER_TOKEN_FILE"
      return 1
    fi
    token_owner="$(sudo stat -c '%u:%g' "$RUNNER_TOKEN_FILE")"
    token_mode="$(sudo stat -c '%a' "$RUNNER_TOKEN_FILE")"
    if [ "$token_owner" != '0:0' ] || [ "$token_mode" != 600 ]; then
      die "existing registration token must be root:root mode 0600: $RUNNER_TOKEN_FILE"
      return 1
    fi
  elif [ -n "$GH_PAT" ]; then
    printf '%s' "$GH_PAT" | sudo install -m 0600 -o root -g root /dev/stdin "$RUNNER_TOKEN_FILE"
  else
    die "put a repository administration PAT in $RUNNER_TOKEN_FILE (root:root, mode 0600), or provide GH_PAT once"
    return 1
  fi
  unset GH_PAT
}

render_runner_loop() {
  local loop
  loop="$(
    cat <<'LOOP'
#!/usr/bin/env bash
set -euo pipefail
index="${1:?runner index required}"
runner_user="__RUNNER_USER__"
runner_group="__RUNNER_USER__"
runner_base="__RUNNER_BASE__"
token_file="__RUNNER_TOKEN_FILE__"
runner_archive="__RUNNER_ARCHIVE__"
repo="__GH_REPO__"
labels="__RUNNER_LABELS__"
cpus="__RUNNER_CPUS__"
dir="$runner_base/runner-$index"
job_home="$dir/home"
work="$dir/_work"
minimal_path="/nix/var/nix/profiles/per-user/thegn-ci/profile/bin:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
while :; do
  # Recreate the writable runner tree from the root-owned archive every job.
  rm -rf -- "$dir"
  install -d -m 0755 "$dir"
  tar xzf "$runner_archive" -C "$dir"
  cp "$runner_base/runner.env" "$dir/.env"
  chown -R "$runner_user:$runner_group" "$dir"
  rm -rf -- "$work" "$job_home"
  install -d -m 0700 -o "$runner_user" -g "$runner_group" "$work" "$job_home"
  epoch="$(date +%s)"
  runner_name="ageless-$index-$epoch"
  label_json="$(printf '%s' "$labels" | jq -R 'split(",")')"
  payload="$(jq -nc --arg name "$runner_name" --argjson labels "$label_json" \
    '{name:$name,runner_group_id:1,labels:$labels,work_folder:"_work"}')"
  token="$(cat "$token_file")"
  jit="$(curl -fsS --max-time 30 -X POST \
    -H "Authorization: Bearer $token" \
    -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$repo/actions/runners/generate-jitconfig" \
    -d "$payload" | jq -er '.encoded_jit_config')"
  unset token payload label_json
  # --jitconfig is single-use and rides argv only in the thegn-ci-owned process.
  systemd-run --wait --collect --pipe --uid="$runner_user" --gid="$runner_group" \
    -p "CPUQuota=$((cpus * 100))%" -p MemoryHigh=12G \
    -p "WorkingDirectory=$dir" --setenv="HOME=$job_home" --setenv="PATH=$minimal_path" \
    -- "$dir/run.sh" --jitconfig "$jit"
  unset jit
done
LOOP
  )"
  loop="${loop//__GH_REPO__/$GH_REPO}"
  loop="${loop//__RUNNER_LABELS__/$RUNNER_LABELS}"
  loop="${loop//__RUNNER_CPUS__/$RUNNER_CPUS}"
  loop="${loop//__RUNNER_USER__/$RUNNER_USER}"
  loop="${loop//__RUNNER_BASE__/$RUNNER_BASE}"
  loop="${loop//__RUNNER_TOKEN_FILE__/$RUNNER_TOKEN_FILE}"
  loop="${loop//__RUNNER_ARCHIVE__/$RUNNER_ARCHIVE_FILE}"
  printf '%s\n' "$loop"
}

render_runner_unit() {
  local unit
  unit="$(
    cat <<'UNIT'
[Unit]
Description=Ephemeral thegn CI runner %i
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
Restart=always
RestartSec=5
ExecStart=__RUNNER_LOOP_FILE__ %i

[Install]
WantedBy=multi-user.target
UNIT
  )"
  unit="${unit//__RUNNER_LOOP_FILE__/$RUNNER_LOOP_FILE}"
  printf '%s\n' "$unit"
}

render_runner_env() {
  cat <<'ENV'
DO_NOT_TRACK=1
OPENSPEC_TELEMETRY=0
ENV
}

install_runner_loop() {
  local loop unit tmp_loop tmp_unit
  loop="$(render_runner_loop)"
  unit="$(render_runner_unit)"
  tmp_loop="$(mktemp)"
  tmp_unit="$(mktemp)"
  printf '%s\n' "$loop" >"$tmp_loop"
  printf '%s\n' "$unit" >"$tmp_unit"
  sudo install -D -m 0755 -o root -g root "$tmp_loop" "$RUNNER_LOOP_FILE"
  sudo install -D -m 0644 -o root -g root "$tmp_unit" "$RUNNER_UNIT_FILE"
  rm -f "$tmp_loop" "$tmp_unit"
  sudo systemctl daemon-reload
  local index
  for ((index = 1; index <= RUNNER_COUNT; index++)); do
    sudo systemctl enable --now "thegn-ci-runner@$index.service"
  done
}

install_nix_if_missing() {
  if ! command -v nix >/dev/null && [ ! -e /nix/var/nix/profiles/default/bin/nix ]; then
    log 'installing nix (Determinate Systems, multi-user)…'
    curl -fsSL https://install.determinate.systems/nix | sh -s -- install --no-confirm
  else
    log 'nix already installed — leaving it in place'
  fi
}

install_runner_payload() {
  local version tarball cache_tarball tmp_archive tmp_env base_owner base_mode
  if sudo test -L "$RUNNER_BASE"; then
    die "runner base is a symlink: $RUNNER_BASE"
    return 1
  fi
  sudo install -d -m 0755 -o root -g root "$RUNNER_BASE"
  base_owner="$(sudo stat -c '%u:%g' "$RUNNER_BASE")"
  base_mode="$(sudo stat -c '%a' "$RUNNER_BASE")"
  if [ "$base_owner" != '0:0' ] || [ "$base_mode" != 755 ]; then
    die "runner base must be root:root mode 0755: $RUNNER_BASE"
    return 1
  fi
  tmp_env="$(mktemp)"
  render_runner_env >"$tmp_env"
  sudo install -m 0644 -o root -g root "$tmp_env" "$RUNNER_BASE/runner.env"
  rm -f "$tmp_env"
  version="$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | jq -r .tag_name | sed 's/^v//')"
  tarball="actions-runner-linux-x64-${version}.tar.gz"
  cache_tarball="$RUNNER_BASE/$tarball"
  RUNNER_ARCHIVE_FILE="$cache_tarball"
  if ! sudo test -f "$cache_tarball"; then
    log "downloading actions-runner v$version…"
    tmp_archive="$(mktemp)"
    curl -fsSL -o "$tmp_archive" "https://github.com/actions/runner/releases/download/v${version}/${tarball}"
    sudo install -D -m 0644 -o root -g root "$tmp_archive" "$cache_tarball"
    rm -f "$tmp_archive"
  fi
}

main() {
  unexport_credentials
  if [ "$(id -u)" = 0 ]; then
    die 'run setup as an operator, never as root'
    return 1
  fi
  runner_preflight
  command -v systemctl >/dev/null || {
    die 'systemd is required'
    return 1
  }
  for tool in curl tar jq; do command -v "$tool" >/dev/null || {
    die "missing '$tool'"
    return 1
  }; done
  ensure_runner_account
  runner_preflight
  secure_runner_home_parent
  prepare_registration_token
  install_nix_if_missing
  install_runner_payload
  install_runner_loop
  log "done. Verify in GitHub Settings → Actions → Runners (expect $RUNNER_COUNT ephemeral runners)."
}

if [ "${THEGN_CI_SETUP_SOURCE_ONLY:-0}" != 1 ]; then main "$@"; fi
