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
THEGN_CI_SETUP_SOURCE_ONLY=1
export THEGN_CI_SETUP_SOURCE_ONLY
source "$ROOT/scripts/ci/ageless-runner-setup.sh"

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
assert_case runner_loop_never_passes_token_to_job case_runner_no_token_to_job
assert_case runner_env_has_no_shared_cargo_cache case_runner_env_clean
assert_case unit_runs_loop_as_root_template case_unit_root

if [ "$failures" -ne 0 ]; then
  exit 1
fi
