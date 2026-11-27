#!/usr/bin/env bash
# Configure root-owned JIT runner loops. Run this script as an operator; jobs
# execute as the isolated, non-login thegn-ci account.
set -euo pipefail

# shellcheck source=scripts/lib/bootstrap-artifacts.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../lib/bootstrap-artifacts.sh"

# ── Reviewed bootstrap identities ───────────────────────────────────────────
# Every executable byte this script introduces is pinned here by release, exact
# size, and SHA-256. "latest" is never resolved at run time, so a mutable
# upstream response cannot silently change what runs as root. Refreshing a pin is
# an explicit reviewed edit: bump the version, size, digest, and byte caps
# together from the provenance recorded beside each pin, then re-run this script.
# A cached artifact is re-authenticated on every run; an existing file is never
# evidence of integrity. See docs/self-hosted-ci.md for the update procedure.
#
# The only origins a pinned download may end on are GitHub's release hosts; a
# redirect anywhere else is refused before the body is authenticated.
GITHUB_RELEASE_HOSTS="github.com objects.githubusercontent.com release-assets.githubusercontent.com"
#
# Determinate Nix installer, x86_64-linux. Provenance: the GitHub release asset
# metadata for DeterminateSystems/nix-installer, re-hashed on download here.
NIX_INSTALLER_VERSION="v3.24.2"
NIX_INSTALLER_URL="https://github.com/DeterminateSystems/nix-installer/releases/download/${NIX_INSTALLER_VERSION}/nix-installer-x86_64-linux"
NIX_INSTALLER_SHA256="22ed14ee1b24d100d388f1fc245e3062bd5982433d159cffa6dec34c0eb15dab"
NIX_INSTALLER_SIZE=78449072
NIX_INSTALLER_MAX_BYTES=100663296
# GitHub Actions runner, linux-x64. Provenance: the GitHub release asset `digest`
# for actions/runner's pinned tag, re-hashed on download here. The reviewed tree
# expands to 684215259 bytes across 11556 members; the caps leave headroom for a
# reviewed refresh without letting an unbounded archive through.
RUNNER_VERSION="2.338.2"
RUNNER_TARBALL="actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
RUNNER_URL="https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/${RUNNER_TARBALL}"
RUNNER_SHA256="baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40"
RUNNER_SIZE=228071396
RUNNER_MAX_BYTES=268435456
RUNNER_MAX_EXPANDED_BYTES=805306368
RUNNER_MAX_MEMBERS=32768
RUNNER_REQUIRED_MEMBERS="run.sh config.sh bin/Runner.Listener"

GH_REPO="${GH_REPO:-blakeashleyjr/thegn}"
RUNNER_USER="${RUNNER_USER:-thegn-ci}"
RUNNER_COUNT="${RUNNER_COUNT:-4}"
RUNNER_CPUS="${RUNNER_CPUS:-3}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,linux,x64,ageless,nix}"
RUNNER_BASE="${RUNNER_BASE:-/var/lib/thegn-ci/runners}"
RUNNER_TOKEN_FILE="${RUNNER_TOKEN_FILE:-/etc/thegn-ci/registration.token}"
RUNNER_LOOP_FILE="${RUNNER_LOOP_FILE:-/usr/local/libexec/thegn-ci/runner-loop}"
RUNNER_UNIT_FILE="${RUNNER_UNIT_FILE:-/etc/systemd/system/thegn-ci-runner@.service}"
RUNNER_ARCHIVE_FILE="${RUNNER_ARCHIVE_FILE:-$RUNNER_BASE/$RUNNER_TARBALL}"
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
  local failures=0 groups shell home sudo_output sudo_status account_line path
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
    local group
    for group in sudo wheel admin docker podman libvirt kvm lxd; do
      if [[ " $groups " == *" $group "* ]]; then
        printf 'runner account %s belongs to privileged group %s (fix: remove it with usermod -G)\n' "$RUNNER_USER" "$group" >&2
        failures=1
      fi
    done
  fi
  sudo_status=0
  sudo_output="$(LC_ALL=C sudo -n -l -U "$RUNNER_USER" 2>&1)" || sudo_status=$?
  if [[ $sudo_output == *"is not allowed to run sudo"* ]]; then
    : # A clean policy may return either status depending on the sudo build.
  elif [ "$sudo_status" -ne 0 ]; then
    printf 'cannot verify sudo rights for runner account %s (fix: make sudo -n -l -U succeed, then remove any grants)\n' "$RUNNER_USER" >&2
    failures=1
  else
    printf 'runner account %s has sudo rights (fix: remove its sudoers grant)\n' "$RUNNER_USER" >&2
    failures=1
  fi
  if ! sudo test -x "$home" || ! sudo test -r "$home"; then
    printf 'cannot inspect runner HOME %s (fix: restore root traversal/read access and retry)\n' "$home" >&2
    failures=1
  fi
  for path in "$home/.config/gh/hosts.yml" "$home/.ssh" "$home/.gnupg" \
    "$home/.git-credentials" "$home/.config/nix/nix.conf" "$home/.docker/config.json"; do
    if sudo test -e "$path" || sudo test -L "$path"; then
      if [[ $path == */nix.conf ]]; then
        if sudo grep -qi 'access-tokens' "$path"; then
          :
        else
          local grep_status=$?
          if [ "$grep_status" -ne 1 ]; then
            printf 'cannot inspect runner HOME: unreadable nix.conf at %s\n' "$path" >&2
            failures=1
            continue
          fi
          continue
        fi
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
  if [ -e /etc/NIXOS ]; then
    cat >&2 <<'NIXOS'
NixOS manages users and systemd units declaratively; /etc/systemd/system is generated and read-only.
Add this module to configuration.nix. Provision the rendered loop at the declared ExecStart path
through the Nix store; this script cannot write /etc/systemd/system on NixOS.

{ pkgs, ... }:
{
users.groups.thegn-ci = {};
users.users.thegn-ci = {
  isSystemUser = true;
  group = "thegn-ci";
  home = "/var/lib/thegn-ci";
  createHome = true;
  shell = "${pkgs.shadow}/bin/nologin";
};
systemd.services."thegn-ci-runner@" = {
  description = "Ephemeral thegn CI runner %i";
  wantedBy = [ "multi-user.target" ];
  after = [ "network-online.target" ];
  wants = [ "network-online.target" ];
  serviceConfig = {
    Type = "simple";
    User = "root";
    Group = "root";
    Restart = "always";
    RestartSec = 5;
    ExecStart = "/usr/local/libexec/thegn-ci/runner-loop %i";
  };
};
}

Setup stopped before installing anything.
NIXOS
    return 1
  fi
  if getent passwd "$RUNNER_USER" >/dev/null; then return 0; fi
  sudo useradd --system --user-group --create-home --home-dir /var/lib/thegn-ci --shell /usr/sbin/nologin "$RUNNER_USER"
}

refuse_legacy_runner_layout() {
  local operator_home legacy_units legacy_running
  operator_home="$(getent passwd "$(id -un)" | cut -d: -f6)"
  legacy_units="$(sudo systemctl list-unit-files 'actions.runner.*' --no-legend --no-pager 2>/dev/null || true)"
  legacy_running="$(sudo systemctl list-units 'actions.runner.*' --all --no-legend --no-pager 2>/dev/null || true)"
  if [ -n "$legacy_units" ] || [ -n "$legacy_running" ]; then
    printf 'legacy actions.runner.* units are installed or loaded:\n%s\n%s\n' "$legacy_units" "$legacy_running" >&2
    printf 'remove them with their old svc.sh stop/uninstall and config.sh remove steps before installing the hardened runner\n' >&2
    return 1
  fi
  if [ -e "$operator_home/actions-runners" ]; then
    printf 'legacy runner directory exists at %s/actions-runners; remove its old svc.sh services and run config.sh remove before installing the hardened runner\n' "$operator_home" >&2
    return 1
  fi
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
minimal_path="/nix/var/nix/profiles/per-user/__RUNNER_USER__/profile/bin:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
while :; do
  job_unit="thegn-ci-job-$index.service"
  job_state="$(systemctl show -p LoadState --value "$job_unit")"
  if [ "$job_state" = loaded ]; then
    systemctl stop "$job_unit"
  fi
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
  if ! jit="$(
    printf 'header = \"Authorization: Bearer %s\"\n' "$token" |
      curl --config - -fsS --max-time 30 -X POST \
        -H 'Accept: application/vnd.github+json' \
        "https://api.github.com/repos/$repo/actions/runners/generate-jitconfig" \
        -d "$payload" | jq -er '.encoded_jit_config'
  )"; then
    unset token payload label_json
    sleep 30
    continue
  fi
  unset token payload label_json
  # --jitconfig is single-use and rides argv only in the thegn-ci-owned process.
  systemd-run --wait --collect --pipe --unit="thegn-ci-job-$index" --uid="$runner_user" --gid="$runner_group" \
    -p "CPUQuota=$((cpus * 100))%" -p MemoryHigh=12G \
    -p PrivateTmp=yes -p ProtectHome=read-only -p NoNewPrivileges=yes \
    -p "PartOf=thegn-ci-runner@$index.service" \
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
    sudo systemctl restart "thegn-ci-runner@$index.service"
  done
}

install_nix_if_missing() {
  if ! command -v nix >/dev/null && [ ! -e /nix/var/nix/profiles/default/bin/nix ]; then
    log "installing nix (Determinate Systems ${NIX_INSTALLER_VERSION}, multi-user)…"
    run_verified_installer "$NIX_INSTALLER_URL" "$NIX_INSTALLER_SHA256" "$NIX_INSTALLER_SIZE" install --no-confirm
  else
    log 'nix already installed — leaving it in place'
  fi
}

run_verified_installer() {
  local url="$1" expected_sha256="$2" expected_size="$3" status=0 staging installer
  shift 3
  if ! staging="$(bootstrap_staging_dir)"; then
    die 'cannot create a private staging directory for the pinned Nix installer'
    return 1
  fi
  installer="$staging/nix-installer"
  if bootstrap_download_pinned "$url" "$expected_sha256" "$expected_size" \
    "$NIX_INSTALLER_MAX_BYTES" 180 "$installer" 'Nix installer' "$GITHUB_RELEASE_HOSTS"; then
    if ! chmod 0700 "$installer"; then
      rm -rf -- "$staging"
      die 'cannot mark the authenticated Nix installer executable'
      return 1
    fi
    "$installer" "$@" || status=$?
  else
    status=1
  fi
  rm -rf -- "$staging"
  return "$status"
}

# Refuse an archive whose members could escape the extraction root, collide with
# each other, or expand without bound. The digest above proves these bytes are the
# reviewed release; this proves the release layout is still the one reviewed. It
# runs before a single member is extracted.
validate_runner_archive() {
  local archive="$1" names verbose facts key value
  local archived_members expanded member_count refused name
  if [ -L "$archive" ] || [ ! -f "$archive" ]; then
    die "runner archive is missing or a symlink: $archive"
    return 1
  fi
  names="$(mktemp)" || {
    die 'cannot create a listing file for the runner archive'
    return 1
  }
  verbose="$(mktemp)" || {
    rm -f -- "$names"
    die 'cannot create a listing file for the runner archive'
    return 1
  }
  if ! tar -tzf "$archive" >"$names" 2>/dev/null; then
    rm -f -- "$names" "$verbose"
    die 'runner archive is malformed, truncated, or unreadable'
    return 1
  fi
  refused=''
  while IFS= read -r name; do
    case "$name" in
    '') refused='an empty member name' ;;
    /*) refused="absolute member path: $name" ;;
    *[[:space:]]* | *[[:cntrl:]]* | *\\*)
      refused="member name with whitespace, control, or backslash characters: $name"
      ;;
    .. | ../* | */.. | */../*)
      refused="traversal component in member path: $name"
      ;;
    esac
    [ -z "$refused" ] || break
  done <"$names"
  if [ -n "$refused" ]; then
    rm -f -- "$names" "$verbose"
    die "runner archive refused: $refused"
    return 1
  fi
  member_count="$(wc -l <"$names")" || member_count=0
  member_count="${member_count//[[:space:]]/}"
  if [ "${member_count:-0}" -eq 0 ]; then
    rm -f -- "$names" "$verbose"
    die 'runner archive is empty'
    return 1
  fi
  if [ "$member_count" -gt "$RUNNER_MAX_MEMBERS" ]; then
    rm -f -- "$names" "$verbose"
    die "runner archive has $member_count members, above the $RUNNER_MAX_MEMBERS cap"
    return 1
  fi
  if [ -n "$(sort <"$names" | uniq -d)" ]; then
    rm -f -- "$names" "$verbose"
    die 'runner archive contains duplicate member destinations'
    return 1
  fi
  if ! tar -tvzf "$archive" >"$verbose" 2>/dev/null; then
    rm -f -- "$names" "$verbose"
    die 'runner archive member metadata is unreadable'
    return 1
  fi
  # Member names are already known to be whitespace-free, so the verbose listing
  # parses exactly: $1 mode, $3 size, $6 member name, then " -> " for a symlink.
  if ! facts="$(
    awk '
      function contained(base, target,    parts, count, i, part, depth) {
        if (target ~ /^\//) return 0
        depth = 0
        count = split(base "/" target, parts, "/")
        for (i = 1; i <= count; i++) {
          part = parts[i]
          if (part == "" || part == ".") continue
          if (part == "..") {
            if (depth == 0) return 0
            depth--
            continue
          }
          depth++
        }
        return 1
      }
      {
        kind = substr($1, 1, 1)
        name = $6
        size = $3
        if (kind != "-" && kind != "d" && kind != "l") {
          printf "refused member type %s: %s\n", kind, name
          bad = 1
          next
        }
        if (size !~ /^[0-9]+$/) {
          printf "refused unreadable member size: %s\n", name
          bad = 1
          next
        }
        total += size
        members++
        if (kind == "l") {
          split_at = index($0, " -> ")
          target = (split_at > 0) ? substr($0, split_at + 4) : ""
          base = name
          if (sub(/\/[^\/]*$/, "", base) == 0) base = "."
          if (target == "" || !contained(base, target)) {
            printf "refused escaping symlink %s -> %s\n", name, target
            bad = 1
          }
        }
      }
      END {
        printf "members %d\ntotal %d\n", members, total
        exit bad ? 1 : 0
      }
    ' "$verbose"
  )"; then
    printf '%s\n' "$facts" >&2
    rm -f -- "$names" "$verbose"
    die 'runner archive has members this script refuses to extract'
    return 1
  fi
  rm -f -- "$names" "$verbose"
  archived_members=''
  expanded=''
  while read -r key value; do
    case "$key" in
    members) archived_members="$value" ;;
    total) expanded="$value" ;;
    esac
  done <<<"$facts"
  if [[ ! $archived_members =~ ^[0-9]+$ || ! $expanded =~ ^[0-9]+$ ]]; then
    die 'runner archive metadata could not be audited'
    return 1
  fi
  if [ "$archived_members" != "$member_count" ]; then
    die "runner archive listings disagree: $member_count names, $archived_members entries"
    return 1
  fi
  if [ "$expanded" -gt "$RUNNER_MAX_EXPANDED_BYTES" ]; then
    die "runner archive expands to $expanded bytes, above the $RUNNER_MAX_EXPANDED_BYTES-byte cap"
    return 1
  fi
  return 0
}

# Extract a validated archive into a fresh private directory with no privilege and
# no pre-existing path to write through, then prove the staged release is whole.
extract_runner_archive() {
  local archive="$1" destination="$2" required stray
  local -a required_members
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    die "runner extraction target already exists: $destination"
    return 1
  fi
  if ! mkdir -m 0700 "$destination"; then
    die "cannot create the runner extraction staging directory: $destination"
    return 1
  fi
  IFS=' ' read -r -a required_members <<<"$RUNNER_REQUIRED_MEMBERS"
  # No -P and no sudo: tar strips a leading "/" and refuses ".." members itself,
  # and nothing exists yet for a member to be written through.
  if ! tar -xzf "$archive" -C "$destination" --no-same-owner --no-same-permissions 2>/dev/null; then
    die 'runner archive did not extract completely'
    return 1
  fi
  for required in "${required_members[@]}"; do
    if [ -L "$destination/$required" ] || [ ! -f "$destination/$required" ]; then
      die "authenticated runner archive is missing $required"
      return 1
    fi
  done
  stray="$(find "$destination" ! -type f ! -type d ! -type l -print 2>/dev/null | head -n 1)"
  if [ -n "$stray" ]; then
    die "runner archive extracted a non-file entry: $stray"
    return 1
  fi
  return 0
}

# A cached archive is re-authenticated on every run: existence alone never
# confers trust, and a tampered or rolled-back cache is a refusal, not a repair.
authenticate_cached_runner_archive() {
  local archive="$1"
  if bootstrap_verify_bytes "$archive" "$RUNNER_SHA256" "$RUNNER_SIZE" &&
    validate_runner_archive "$archive"; then
    return 0
  fi
  printf '\033[1;31m✗ %s\033[0m\n' "cached runner archive is not the pinned actions-runner v$RUNNER_VERSION release; remove it with 'sudo rm -- $archive' and rerun so the pinned bytes are downloaded again" >&2
  return 1
}

# Download, authenticate, validate, extract and only then promote the payload
# atomically into the root-owned runner base.
stage_runner_archive() {
  local staging archive tree promoted
  if ! staging="$(bootstrap_staging_dir)"; then
    die 'cannot create a private staging directory for the runner archive'
    return 1
  fi
  archive="$staging/$RUNNER_TARBALL"
  if ! bootstrap_download_pinned "$RUNNER_URL" "$RUNNER_SHA256" "$RUNNER_SIZE" \
    "$RUNNER_MAX_BYTES" 1800 "$archive" 'Actions runner archive' "$GITHUB_RELEASE_HOSTS"; then
    rm -rf -- "$staging"
    return 1
  fi
  if ! validate_runner_archive "$archive"; then
    rm -rf -- "$staging"
    return 1
  fi
  tree="$staging/tree"
  if ! extract_runner_archive "$archive" "$tree"; then
    rm -rf -- "$staging"
    return 1
  fi
  rm -rf -- "$tree"
  promoted="$RUNNER_BASE/.$RUNNER_TARBALL.$$.partial"
  if ! sudo install -m 0644 -o root -g root "$archive" "$promoted"; then
    sudo rm -f -- "$promoted"
    rm -rf -- "$staging"
    die "cannot stage the authenticated runner archive under $RUNNER_BASE"
    return 1
  fi
  if ! sudo mv -f -- "$promoted" "$RUNNER_BASE/$RUNNER_TARBALL"; then
    sudo rm -f -- "$promoted"
    rm -rf -- "$staging"
    die 'cannot atomically promote the authenticated runner archive'
    return 1
  fi
  rm -rf -- "$staging"
  log "installed authenticated actions-runner v$RUNNER_VERSION payload"
}

install_runner_payload() {
  local cache_tarball tmp_env base_owner base_mode
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
  cache_tarball="$RUNNER_BASE/$RUNNER_TARBALL"
  if sudo test -L "$cache_tarball"; then
    die "cached runner archive is a symlink: $cache_tarball"
    return 1
  fi
  if sudo test -f "$cache_tarball"; then
    authenticate_cached_runner_archive "$cache_tarball" || return 1
  else
    log "installing actions-runner v$RUNNER_VERSION (pinned size and digest)…"
    stage_runner_archive || return 1
  fi
  RUNNER_ARCHIVE_FILE="$cache_tarball"
}

main() {
  unexport_credentials
  if [ "$(id -u)" = 0 ]; then
    die 'run setup as an operator, never as root'
    return 1
  fi
  sudo -v || {
    die 'sudo authentication is required before setup preflight'
    return 1
  }
  refuse_legacy_runner_layout || return 1
  runner_preflight
  command -v systemctl >/dev/null || {
    die 'systemd is required'
    return 1
  }
  local tool
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
