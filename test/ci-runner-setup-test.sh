#!/usr/bin/env bash
set -euo pipefail
# shellcheck disable=SC2016  # tests compare literal variable syntax in rendered loop text

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$(mktemp -d)"
chmod 700 "$TEST_ROOT"
trap 'rm -rf "$TEST_ROOT"' EXIT
STUB_BIN="$TEST_ROOT/bin"
mkdir -m 700 "$STUB_BIN"
export TEST_ROOT STUB_BIN

cat >"$STUB_BIN/id" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  -un) printf '%s\n' "${TEST_OPERATOR:-operator}" ;;
  -u)
    if [ "${2:-}" = "${TEST_ROOT_USER:-root-runner}" ]; then printf '0\n'; else printf '1001\n'; fi
    ;;
  -nG) printf '%s\n' "${TEST_GROUPS:-thegn-ci}" ;;
  *) exit 90 ;;
esac
STUB
cat >"$STUB_BIN/getent" <<'STUB'
#!/usr/bin/env bash
if [ "$1" != passwd ]; then exit 90; fi
printf '%s:x:1001:1001::%s:%s\n' "$2" "${TEST_HOME:-$TEST_ROOT/home}" "${TEST_SHELL:-/usr/sbin/nologin}"
STUB
cat >"$STUB_BIN/sudo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$TEST_ROOT/sudo-argv"
if [ "$#" -eq 4 ] && [ "$1" = -n ] && [ "$2" = -l ] && [ "$3" = -U ]; then
  case "${TEST_SUDO_PROBE:-clean}" in
    clean) printf 'runner is not allowed to run sudo\n'; exit "${TEST_SUDO_CLEAN_STATUS:-0}" ;;
    rights) printf 'User runner may run the following commands\n'; exit 0 ;;
    failed) printf 'sudo probe unavailable\n' >&2; exit 1 ;;
  esac
fi
if [ "$1" = test ]; then
  shift
  if [ "${TEST_HOME_UNREADABLE:-0}" = 1 ] && [[ ${*: -1} == "$TEST_HOME" ]]; then exit 1; fi
  exec test "$@"
fi
if [ "$1" = grep ]; then
  shift
  exec grep "$@"
fi
# Stage-and-promote tests run unprivileged: drop the owner/group options the
# script passes to install(1) and execute the rest for real in the test root.
if [ "$1" = install ]; then
  shift
  filtered=()
  while (($#)); do
    case "$1" in
      -o | -g) shift 2 ;;
      *) filtered+=("$1"); shift ;;
    esac
  done
  exec install "${filtered[@]}"
fi
if [ "$1" = mv ] || [ "$1" = rm ] || [ "$1" = stat ] || [ "$1" = mkdir ]; then
  exec "$@"
fi
if [ "$1" = systemctl ] && [ "${2:-}" = list-unit-files ]; then
  printf '%s' "${TEST_LEGACY_UNITS:-}"
  exit 0
fi
printf 'unexpected sudo invocation in test: %s\n' "$*" >&2
exit 91
STUB
for command_name in curl systemctl; do
  cat >"$STUB_BIN/$command_name" <<'STUB'
#!/usr/bin/env bash
printf 'unexpected network/system operation in test: %s %s\n' "${0##*/}" "$*" >&2
exit 92
STUB
done
chmod 700 "$STUB_BIN"/*
PATH="$STUB_BIN:$PATH"
export PATH
mkdir -m 700 "$TEST_ROOT/home"
# shellcheck source=test/lib/bootstrap-stubs.sh
export TEST_ROOT STUB_BIN
# shellcheck disable=SC1091 # the checked-out library path is explicit
source "$ROOT/test/lib/bootstrap-stubs.sh"
THEGN_CI_SETUP_SOURCE_ONLY=1
export THEGN_CI_SETUP_SOURCE_ONLY
source "$ROOT/scripts/ci/ageless-runner-setup.sh"

# Fixture archives for the validation tests. Generated with python3 so the
# hostile layouts (traversal, absolute, hardlink, device, duplicate) are exact
# ustar members; tar(1) refuses to create most of them on purpose.
ARCHIVES="$TEST_ROOT/archives"
mkdir -m 700 "$ARCHIVES"
python3 - "$ARCHIVES" <<'PY'
import io
import os
import sys
import tarfile

out = sys.argv[1]


def member(tar, name, kind="file", payload=b"", linkname=""):
    info = tarfile.TarInfo(name)
    info.mtime = 0
    if kind == "file":
        info.mode = 0o755
        info.size = len(payload)
        tar.addfile(info, io.BytesIO(payload))
    elif kind == "dir":
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        tar.addfile(info)
    elif kind == "sym":
        info.type = tarfile.SYMTYPE
        info.linkname = linkname
        info.mode = 0o777
        tar.addfile(info)
    elif kind == "hard":
        info.type = tarfile.LNKTYPE
        info.linkname = linkname
        tar.addfile(info)
    elif kind == "dev":
        info.type = tarfile.CHRTYPE
        tar.addfile(info)


def archive(name, entries):
    with tarfile.open(os.path.join(out, name), "w:gz") as tar:
        for entry in entries:
            member(tar, *entry)


archive(
    "valid.tar.gz",
    [
        ("run.sh", "file", b"#!/bin/sh\nexit 0\n"),
        ("config.sh", "file", b"#!/bin/sh\nexit 0\n"),
        ("bin", "dir"),
        ("bin/Runner.Listener", "file", b"listener\n"),
        ("lib", "dir"),
        ("lib/npm.js", "file", b"module.exports = {};\n"),
        ("bin/npm", "sym", b"", "../lib/npm.js"),
    ],
)
archive("traversal.tar.gz", [("run.sh",), ("../evil",)])
archive("absolute.tar.gz", [("run.sh",), ("/etc/evil",)])
archive("symlink-escape.tar.gz", [("run.sh",), ("link", "sym", b"", "../../etc/passwd")])
archive("hardlink.tar.gz", [("run.sh",), ("h", "hard", b"", "run.sh")])
archive("device.tar.gz", [("run.sh",), ("dev", "dev")])
archive("duplicate.tar.gz", [("run.sh",), ("run.sh",)])
archive("empty.tar.gz", [])
with open(os.path.join(out, "malformed.tar.gz"), "wb") as handle:
    handle.write(b"this is not a tar archive\n")
PY

failures=0
assert_case() {
  local name="$1"
  shift
  if "$@"; then
    printf 'ok %s\n' "$name"
  else
    printf 'FAIL %s\n' "$name" >&2
    failures=$((failures + 1))
  fi
}
refusal_contains() {
  local expected="$1"
  if runner_preflight >"$TEST_ROOT/output" 2>&1; then return 1; fi
  grep -Fq "$expected" "$TEST_ROOT/output"
}
clean_environment() {
  RUNNER_USER=thegn-ci
  NIX_GITHUB_TOKEN=
  TEST_HOME="$TEST_ROOT/home"
  TEST_SHELL=/usr/sbin/nologin
  TEST_GROUPS=thegn-ci
  TEST_SUDO_PROBE=clean
  TEST_SUDO_CLEAN_STATUS=0
  TEST_LEGACY_UNITS=
  TEST_ROOT_USER=root-runner
  export RUNNER_USER NIX_GITHUB_TOKEN TEST_HOME TEST_SHELL TEST_GROUPS TEST_SUDO_PROBE TEST_SUDO_CLEAN_STATUS TEST_ROOT_USER TEST_LEGACY_UNITS
  rm -rf -- "${TEST_ROOT:?}/home"
  mkdir -m 700 "$TEST_ROOT/home"
}

case_runner_equal_operator() {
  clean_environment
  RUNNER_USER=operator
  export RUNNER_USER
  refusal_contains 'is the invoking operator'
}
case_root_runner() {
  clean_environment
  RUNNER_USER=root-runner
  export RUNNER_USER
  refusal_contains 'is root'
}
case_sudo_group() {
  clean_environment
  TEST_GROUPS='thegn-ci sudo'
  export TEST_GROUPS
  refusal_contains 'privileged group sudo'
}
case_sudo_rights() {
  clean_environment
  TEST_SUDO_PROBE=rights
  export TEST_SUDO_PROBE
  refusal_contains 'has sudo rights'
}
case_sudo_failed() {
  clean_environment
  TEST_SUDO_PROBE=failed
  export TEST_SUDO_PROBE
  refusal_contains 'cannot verify sudo rights'
}
case_sudo_clean_nonzero() {
  clean_environment
  TEST_SUDO_CLEAN_STATUS=1
  export TEST_SUDO_CLEAN_STATUS
  runner_preflight >"$TEST_ROOT/output" 2>&1
}
case_login_shell() {
  clean_environment
  TEST_SHELL=/bin/bash
  export TEST_SHELL
  refusal_contains 'has interactive shell'
}
case_gh_hosts() {
  clean_environment
  mkdir -p "$TEST_ROOT/home/.config/gh"
  touch "$TEST_ROOT/home/.config/gh/hosts.yml"
  refusal_contains 'credentials or persistent auth state'
}
case_nix_access_tokens() {
  clean_environment
  mkdir -p "$TEST_ROOT/home/.config/nix"
  printf 'access-tokens = github.com=secret\n' >"$TEST_ROOT/home/.config/nix/nix.conf"
  refusal_contains 'credentials or persistent auth state'
}
case_nix_token_env() {
  clean_environment
  NIX_GITHUB_TOKEN=secret
  export NIX_GITHUB_TOKEN
  refusal_contains 'NIX_GITHUB_TOKEN is set'
}
case_clean_runner() {
  clean_environment
  runner_preflight >"$TEST_ROOT/output" 2>&1
}
case_unreadable_home() {
  clean_environment
  chmod 000 "$TEST_ROOT/home"
  TEST_HOME_UNREADABLE=1
  export TEST_HOME_UNREADABLE
  refusal_contains 'cannot inspect runner HOME'
}
case_legacy_units() {
  clean_environment
  TEST_LEGACY_UNITS='actions.runner.old.service enabled'
  export TEST_LEGACY_UNITS
  if refuse_legacy_runner_layout >"$TEST_ROOT/output" 2>&1; then return 1; fi
  grep -Fq 'legacy actions.runner.* units are installed' "$TEST_ROOT/output"
}
case_legacy_home() {
  clean_environment
  mkdir "$TEST_ROOT/home/actions-runners"
  if refuse_legacy_runner_layout >"$TEST_ROOT/output" 2>&1; then return 1; fi
  grep -Fq 'legacy runner directory exists' "$TEST_ROOT/output"
}
# shellcheck disable=SC2016  # literal text of the rendered loop
case_runner_wipes() {
  local rendered
  rendered="$(render_runner_loop)"
  grep -Fq 'rm -rf -- "$work" "$job_home"' <<<"$rendered" &&
    grep -Fq 'install -d -m 0700 -o "$runner_user" -g "$runner_group" "$work" "$job_home"' <<<"$rendered"
}
# shellcheck disable=SC2016  # literal text of the rendered loop
case_runner_jit() {
  local rendered
  rendered="$(render_runner_loop)"
  grep -Fq '/generate-jitconfig' <<<"$rendered" &&
    grep -Fq -- '--jitconfig "$jit"' <<<"$rendered" &&
    ! grep -Fq -- '--replace' <<<"$rendered"
}
# shellcheck disable=SC2016  # literal text of the rendered loop
case_runner_unit() {
  local rendered
  rendered="$(render_runner_loop)"
  grep -Fq 'systemd-run --wait' <<<"$rendered" &&
    grep -Fq -- '--uid="$runner_user"' <<<"$rendered" &&
    grep -Fq -- '--unit="thegn-ci-job-$index"' <<<"$rendered" &&
    grep -Fq -- '-p PrivateTmp=yes -p ProtectHome=read-only -p NoNewPrivileges=yes' <<<"$rendered"
}
case_runner_no_token_to_job() {
  local rendered job_line
  rendered="$(render_runner_loop)"
  job_line="$(printf '%s\n' "$rendered" | grep 'systemd-run --wait')"
  [[ $job_line != *token* && $job_line != *GH_PAT* && $job_line != *NIX_GITHUB_TOKEN* ]]
}
stub_curl_payload() {
  bootstrap_stub_curl
}
# A valid fixture archive with the pins every runner artifact check reads.
runner_fixture_env() {
  RUNNER_BASE="$TEST_ROOT/base"
  RUNNER_TARBALL="actions-runner-fixture.tar.gz"
  RUNNER_URL="https://github.com/actions/runner/releases/download/v0/fixture.tar.gz"
  RUNNER_SIZE="$(wc -c <"$ARCHIVES/valid.tar.gz" | tr -d '[:space:]')"
  RUNNER_SHA256="$(bootstrap_sha256_file "$ARCHIVES/valid.tar.gz")"
  RUNNER_MAX_BYTES="$RUNNER_SIZE"
  RUNNER_MAX_EXPANDED_BYTES=4096
  RUNNER_MAX_MEMBERS=32
  RUNNER_REQUIRED_MEMBERS="run.sh config.sh bin/Runner.Listener"
  rm -rf -- "$RUNNER_BASE"
  mkdir -p "$RUNNER_BASE"
}
payload_pins() {
  TEST_PAYLOAD="$ARCHIVES/valid.tar.gz"
  TEST_EFFECTIVE_URL="$RUNNER_URL"
  TEST_CURL_FAIL=0
  export TEST_PAYLOAD TEST_EFFECTIVE_URL TEST_CURL_FAIL
  bootstrap_stub_curl
}
case_verified_installer_runs_matching_bytes() {
  clean_environment
  rm -f -- "$TEST_ROOT/installer-ran"
  local payload_sha payload_size
  cat >"$TEST_ROOT/installer-fixture" <<'FIXTURE'
#!/bin/sh
printf '%s\n' "$*" >"$TEST_ROOT/installer-ran"
FIXTURE
  payload_sha="$(bootstrap_sha256_file "$TEST_ROOT/installer-fixture")"
  payload_size="$(wc -c <"$TEST_ROOT/installer-fixture" | tr -d '[:space:]')"
  TEST_PAYLOAD="$TEST_ROOT/installer-fixture"
  TEST_EFFECTIVE_URL="https://github.com/DeterminateSystems/nix-installer/releases/download/v3.24.2/nix-installer-x86_64-linux"
  export TEST_PAYLOAD TEST_EFFECTIVE_URL
  stub_curl_payload
  run_verified_installer https://github.com/DeterminateSystems/nix-installer/releases/download/v3.24.2/nix-installer-x86_64-linux "$payload_sha" "$payload_size" install --no-confirm || return 1
  [[ $(<"$TEST_ROOT/installer-ran") == 'install --no-confirm' ]]
}
# shellcheck disable=SC2016  # assertion matches shell source with literal variable names
case_nix_bootstrap_uses_reviewed_pin() {
  grep -Fq 'NIX_INSTALLER_VERSION="v3.24.2"' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    grep -Fq 'NIX_INSTALLER_SHA256="22ed14ee1b24d100d388f1fc245e3062bd5982433d159cffa6dec34c0eb15dab"' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    grep -Fq 'NIX_INSTALLER_SIZE=78449072' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    grep -Fq 'run_verified_installer "$NIX_INSTALLER_URL" "$NIX_INSTALLER_SHA256" "$NIX_INSTALLER_SIZE" install --no-confirm' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    ! grep -Fq 'curl -fsSL https://install.determinate.systems/nix | sh' "$ROOT/scripts/ci/ageless-runner-setup.sh"
}
# shellcheck disable=SC2016  # assertion matches shell source with literal variable names
case_runner_bootstrap_uses_reviewed_pin() {
  grep -Fq 'RUNNER_VERSION="2.338.2"' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    grep -Fq 'RUNNER_SHA256="baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40"' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    grep -Fq 'RUNNER_SIZE=228071396' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    grep -Fq 'bootstrap_download_pinned "$RUNNER_URL" "$RUNNER_SHA256" "$RUNNER_SIZE"' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    ! grep -Fq 'releases/latest' "$ROOT/scripts/ci/ageless-runner-setup.sh" &&
    ! grep -Fq 'sudo ./bin/installdependencies.sh' "$ROOT/scripts/ci/ageless-runner-setup.sh"
}
case_verified_installer_rejects_bad_digest_before_execution() {
  clean_environment
  rm -f -- "$TEST_ROOT/installer-ran"
  printf '#!/bin/sh\ntouch "%s/installer-ran"\n' "$TEST_ROOT" >"$TEST_ROOT/installer-fixture"
  TEST_PAYLOAD="$TEST_ROOT/installer-fixture"
  TEST_EFFECTIVE_URL="https://github.com/fixture/installer"
  export TEST_PAYLOAD TEST_EFFECTIVE_URL
  stub_curl_payload
  if run_verified_installer https://github.com/fixture/installer "$(printf '0%.0s' {1..64})" "$(wc -c <"$TEST_ROOT/installer-fixture" | tr -d '[:space:]')" install; then return 1; fi
  [[ ! -e $TEST_ROOT/installer-ran ]]
}
case_verified_installer_rejects_oversized_bytes_before_execution() {
  clean_environment
  rm -f -- "$TEST_ROOT/installer-ran"
  printf '#!/bin/sh\ntouch "%s/installer-ran"\n' "$TEST_ROOT" >"$TEST_ROOT/installer-fixture"
  TEST_PAYLOAD="$TEST_ROOT/installer-fixture"
  TEST_EFFECTIVE_URL="https://github.com/fixture/installer"
  export TEST_PAYLOAD TEST_EFFECTIVE_URL
  stub_curl_payload
  if run_verified_installer https://github.com/fixture/installer "$(printf '0%.0s' {1..64})" "$((NIX_INSTALLER_MAX_BYTES + 1))" install; then return 1; fi
  [[ ! -e $TEST_ROOT/installer-ran ]]
}
case_download_refuses_non_https() {
  clean_environment
  local destination="$TEST_ROOT/non-https"
  if bootstrap_download_pinned "http://github.com/fixture" "$(printf '0%.0s' {1..64})" 1 16 5 "$destination" 'Fixture' "$GITHUB_RELEASE_HOSTS"; then return 1; fi
  [[ ! -e $destination ]]
}
case_download_refuses_unapproved_redirect() {
  clean_environment
  runner_fixture_env
  payload_pins
  TEST_EFFECTIVE_URL="https://evil.invalid/github-production-release-asset/fixture.tar.gz"
  export TEST_EFFECTIVE_URL
  local destination="$TEST_ROOT/redirected"
  if bootstrap_download_pinned "$RUNNER_URL" "$RUNNER_SHA256" "$RUNNER_SIZE" "$RUNNER_MAX_BYTES" 5 "$destination" 'Fixture' "$GITHUB_RELEASE_HOSTS" >"$TEST_ROOT/output" 2>&1; then return 1; fi
  grep -Fq 'unapproved origin' "$TEST_ROOT/output"
}
case_download_accepts_approved_redirect() {
  clean_environment
  runner_fixture_env
  payload_pins
  TEST_EFFECTIVE_URL="https://release-assets.githubusercontent.com/github-production-release-asset/fixture.tar.gz"
  export TEST_EFFECTIVE_URL
  local destination="$TEST_ROOT/redirected-ok"
  bootstrap_download_pinned "$RUNNER_URL" "$RUNNER_SHA256" "$RUNNER_SIZE" "$RUNNER_MAX_BYTES" 5 "$destination" 'Fixture' "$GITHUB_RELEASE_HOSTS" || return 1
  [[ $(bootstrap_sha256_file "$destination") == "$RUNNER_SHA256" ]]
}
case_download_bounds_the_transfer() {
  clean_environment
  runner_fixture_env
  payload_pins
  : >"$TEST_ROOT/curl-argv"
  local destination="$TEST_ROOT/bounded" argv
  bootstrap_download_pinned "$RUNNER_URL" "$RUNNER_SHA256" "$RUNNER_SIZE" "$RUNNER_MAX_BYTES" 5 "$destination" 'Fixture' "$GITHUB_RELEASE_HOSTS" || return 1
  argv="$(<"$TEST_ROOT/curl-argv")"
  [[ $argv == *'--connect-timeout 30'* && $argv == *'--speed-time 60'* && $argv == *'--max-time 5'* && $argv == *"--max-filesize $RUNNER_MAX_BYTES"* && $argv == *'--proto-redir =https'* ]]
}
case_download_refuses_truncated_body() {
  clean_environment
  runner_fixture_env
  payload_pins
  local destination="$TEST_ROOT/truncated"
  if bootstrap_download_pinned "$RUNNER_URL" "$RUNNER_SHA256" "$((RUNNER_SIZE + 1))" "$((RUNNER_SIZE + 1))" 5 "$destination" 'Fixture' "$GITHUB_RELEASE_HOSTS" >"$TEST_ROOT/output" 2>&1; then return 1; fi
  grep -Fq 'size mismatch' "$TEST_ROOT/output"
}
case_download_refuses_interrupted_transfer() {
  clean_environment
  runner_fixture_env
  payload_pins
  TEST_CURL_FAIL=1
  export TEST_CURL_FAIL
  local destination="$TEST_ROOT/interrupted"
  if bootstrap_download_pinned "$RUNNER_URL" "$RUNNER_SHA256" "$RUNNER_SIZE" "$RUNNER_MAX_BYTES" 5 "$destination" 'Fixture' "$GITHUB_RELEASE_HOSTS" >"$TEST_ROOT/output" 2>&1; then return 1; fi
  grep -Fq 'could not download' "$TEST_ROOT/output"
}
case_cached_archive_is_reauthenticated() {
  clean_environment
  runner_fixture_env
  local cache="$TEST_ROOT/cache-$RUNNER_TARBALL"
  cp -- "$ARCHIVES/valid.tar.gz" "$cache"
  authenticate_cached_runner_archive "$cache" >/dev/null 2>&1 || return 1
  # Same size, different bytes: only the digest can catch this, and existence
  # must never stand in for it.
  printf 'X' | dd of="$cache" bs=1 seek=30 conv=notrunc 2>/dev/null
  if authenticate_cached_runner_archive "$cache" >/dev/null 2>&1; then return 1; fi
  cp -- "$ARCHIVES/valid.tar.gz" "$cache"
  authenticate_cached_runner_archive "$cache" >/dev/null 2>&1
}
refuses_archive() {
  local archive="$1" expected="$2"
  if validate_runner_archive "$archive" >"$TEST_ROOT/output" 2>&1; then return 1; fi
  grep -Fq "$expected" "$TEST_ROOT/output"
}
# validate_runner_archive parses `tar -tvzf` columns, which is the GNU layout the
# Linux setup host has. bsdtar (a macOS dev machine) orders the verbose columns
# differently, so those cases are skipped there rather than asserting a layout
# the runtime target never sees.
gnu_tar_listing() {
  [[ $(tar --version 2>/dev/null) == *"GNU tar"* ]]
}
case_archive_accepts_reviewed_layout() {
  clean_environment
  validate_runner_archive "$ARCHIVES/valid.tar.gz" >/dev/null 2>&1
}
case_archive_refuses_traversal() {
  clean_environment
  refuses_archive "$ARCHIVES/traversal.tar.gz" 'traversal component'
}
case_archive_refuses_absolute_path() {
  clean_environment
  refuses_archive "$ARCHIVES/absolute.tar.gz" 'absolute member path'
}
case_archive_refuses_escaping_symlink() {
  clean_environment
  refuses_archive "$ARCHIVES/symlink-escape.tar.gz" 'escaping symlink'
}
case_archive_refuses_hardlink() {
  clean_environment
  refuses_archive "$ARCHIVES/hardlink.tar.gz" 'refused member type h'
}
case_archive_refuses_device() {
  clean_environment
  refuses_archive "$ARCHIVES/device.tar.gz" 'refused member type c'
}
case_archive_refuses_duplicate_destination() {
  clean_environment
  refuses_archive "$ARCHIVES/duplicate.tar.gz" 'duplicate member destinations'
}
case_archive_refuses_malformed() {
  clean_environment
  refuses_archive "$ARCHIVES/malformed.tar.gz" 'malformed'
}
case_archive_refuses_empty() {
  clean_environment
  refuses_archive "$ARCHIVES/empty.tar.gz" 'is empty'
}
case_archive_refuses_excess_expanded_size() {
  clean_environment
  local saved="$RUNNER_MAX_EXPANDED_BYTES"
  RUNNER_MAX_EXPANDED_BYTES=1
  if validate_runner_archive "$ARCHIVES/valid.tar.gz" >"$TEST_ROOT/output" 2>&1; then
    RUNNER_MAX_EXPANDED_BYTES="$saved"
    return 1
  fi
  RUNNER_MAX_EXPANDED_BYTES="$saved"
  grep -Fq 'expands to' "$TEST_ROOT/output"
}
case_stage_promotes_authenticated_archive() {
  clean_environment
  runner_fixture_env
  payload_pins
  : >"$TEST_ROOT/sudo-argv"
  stage_runner_archive >/dev/null 2>&1 || return 1
  [[ -f $RUNNER_BASE/$RUNNER_TARBALL ]] || return 1
  [[ $(bootstrap_sha256_file "$RUNNER_BASE/$RUNNER_TARBALL") == "$RUNNER_SHA256" ]] || return 1
  [[ -z $(find "$RUNNER_BASE" -name '*.partial' -print -quit) ]] || return 1
  grep -Fq 'install -m 0644' "$TEST_ROOT/sudo-argv" &&
    grep -Fq 'mv -f' "$TEST_ROOT/sudo-argv"
}
case_stage_refuses_wrong_digest_before_promotion() {
  clean_environment
  runner_fixture_env
  payload_pins
  RUNNER_SHA256="$(printf '0%.0s' {1..64})"
  if stage_runner_archive >/dev/null 2>&1; then return 1; fi
  [[ ! -e $RUNNER_BASE/$RUNNER_TARBALL ]]
}
case_runner_env_clean() {
  local rendered
  rendered="$(render_runner_env)"
  [[ $rendered == $'DO_NOT_TRACK=1\nOPENSPEC_TELEMETRY=0' && $rendered != *CARGO_HOME=* && $rendered != *SCCACHE_DIR=* && $rendered != *'gha-cache/cargo/bin'* ]]
}
case_runner_token_off_argv() {
  local rendered
  rendered="$(render_runner_loop)"
  # shellcheck disable=SC2016  # token and Bearer are literal argv leak markers
  ! grep -E 'curl.*(\$token|Bearer)|(\$token|Bearer).*curl' <<<"$rendered"
}
case_unit_root() {
  local rendered
  rendered="$(render_runner_unit)"
  [[ $rendered == *'User=root'* && $rendered == *'ExecStart=/usr/local/libexec/thegn-ci/runner-loop %i'* ]]
}

assert_case preflight_refuses_runner_equal_to_operator case_runner_equal_operator
assert_case preflight_refuses_root_runner case_root_runner
assert_case preflight_refuses_sudo_group case_sudo_group
assert_case preflight_refuses_sudo_rights case_sudo_rights
assert_case preflight_refuses_failed_sudo_probe case_sudo_failed
assert_case preflight_accepts_clean_sudo_probe_with_nonzero_status case_sudo_clean_nonzero
assert_case preflight_refuses_login_shell case_login_shell
assert_case preflight_refuses_gh_hosts_file case_gh_hosts
assert_case preflight_refuses_nix_access_tokens case_nix_access_tokens
assert_case preflight_refuses_nix_github_token_env case_nix_token_env
assert_case preflight_accepts_clean_runner case_clean_runner
assert_case preflight_refuses_unreadable_home case_unreadable_home
assert_case preflight_refuses_legacy_units case_legacy_units
assert_case preflight_refuses_legacy_home case_legacy_home
assert_case runner_loop_wipes_work_and_home case_runner_wipes
assert_case runner_loop_uses_jitconfig_not_replace case_runner_jit
assert_case runner_loop_runs_job_in_transient_unit case_runner_unit
assert_case runner_loop_keeps_token_off_argv case_runner_token_off_argv
assert_case pinned_installer_executes_only_matching_bytes case_verified_installer_runs_matching_bytes
assert_case nix_bootstrap_uses_reviewed_version_size_and_digest case_nix_bootstrap_uses_reviewed_pin
assert_case pinned_installer_rejects_wrong_digest_before_execution case_verified_installer_rejects_bad_digest_before_execution
assert_case pinned_installer_rejects_oversize_before_execution case_verified_installer_rejects_oversized_bytes_before_execution
assert_case runner_bootstrap_uses_reviewed_version_size_and_digest case_runner_bootstrap_uses_reviewed_pin
assert_case download_refuses_non_https_url case_download_refuses_non_https
assert_case download_refuses_unapproved_redirect_origin case_download_refuses_unapproved_redirect
assert_case download_accepts_approved_redirect_origin case_download_accepts_approved_redirect
assert_case download_bounds_timeouts_and_size case_download_bounds_the_transfer
assert_case download_refuses_truncated_body case_download_refuses_truncated_body
assert_case download_refuses_interrupted_transfer case_download_refuses_interrupted_transfer
if gnu_tar_listing; then
  assert_case cached_archive_is_reauthenticated_not_trusted_by_existence case_cached_archive_is_reauthenticated
  assert_case archive_accepts_reviewed_layout case_archive_accepts_reviewed_layout
  assert_case archive_refuses_traversal case_archive_refuses_traversal
  assert_case archive_refuses_absolute_path case_archive_refuses_absolute_path
  assert_case archive_refuses_escaping_symlink case_archive_refuses_escaping_symlink
  assert_case archive_refuses_hardlink case_archive_refuses_hardlink
  assert_case archive_refuses_device case_archive_refuses_device
  assert_case archive_refuses_duplicate_destination case_archive_refuses_duplicate_destination
  assert_case archive_refuses_malformed case_archive_refuses_malformed
  assert_case archive_refuses_empty case_archive_refuses_empty
  assert_case archive_refuses_excess_expanded_size case_archive_refuses_excess_expanded_size
  assert_case stage_promotes_only_authenticated_archive case_stage_promotes_authenticated_archive
  assert_case stage_refuses_wrong_digest_before_promotion case_stage_refuses_wrong_digest_before_promotion
else
  printf 'skip 13 archive/staging cases: GNU tar verbose listing is required and the runtime target is Linux\n'
fi
assert_case runner_loop_never_passes_token_to_job case_runner_no_token_to_job
assert_case runner_env_has_no_shared_cargo_cache case_runner_env_clean
assert_case unit_runs_loop_as_root_template case_unit_root

if [ "$failures" -ne 0 ]; then
  exit 1
fi
