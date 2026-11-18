#!/usr/bin/env bash
# WS03-P0-03 — capture the user-namespace / AppArmor probe on ONE host.
# Usage: ws03-p0-03.sh OUTDIR CASE_NAME
#   CASE_NAME  fixture name, e.g. ubuntu2404_restricted, ubuntu2404_profile_loaded,
#              ubuntu2510, ubuntu2604, debian_sysctl0, fedora43, rhel10_max0,
#              wsl2, docker_default_seccomp, gha_ubuntu2404
# Run it once per host of the matrix (copy the script in: it needs only bash,
# coreutils and bwrap). Writes OUTDIR/WS03-P0-03/userns-CASE.json (the fixture,
# user names and home paths redacted) and OUTDIR/WS03-P0-03.evidence.txt.
# Read-only on the host except bounded bwrap/systemd-run probes. The status in
# the JSON is a SUGGESTION: a human assigns the final UsernsStatus.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS03-P0-03
sp_init "$item" "${1:-}"
case_name="${2:-}"
[ -n "$case_name" ] || sp_die "usage: $0 OUTDIR CASE_NAME"
[ "$(uname -s)" = Linux ] || sp_die "Linux only"
sp_need awk sed
bwrap_bin="$(command -v bwrap || true)"
[ -n "$bwrap_bin" ] || sp_note "NOTE: bwrap not on PATH: the case maps to Unknown (treated unavailable)"

tmo=15
fx="$SP_OUT/$item/userns-$case_name.json"

rd() { # rd PATH -> trimmed first line or 'absent'
  if [ -r "$1" ]; then head -n1 "$1" | tr -d '\n'; else printf 'absent'; fi
}
jbool() { if "$@"; then printf true; else printf false; fi; }

RUNS=""
RUN_RC=0
RUN_ERR=""
# run NAME cmd... : bounded run, append a JSON object to RUNS, set RUN_RC/RUN_ERR
run() {
  local name="$1" out err rc=0 o e
  shift
  o="$(mktemp)"
  e="$(mktemp)"
  sp_timeout "$tmo" "$@" >"$o" 2>"$e" </dev/null || rc=$?
  RUN_RC=$rc
  RUN_ERR="$(sp_redact <"$e")"
  out="$(sp_redact <"$o" | sp_json_str)"
  err="$(printf '%s' "$RUN_ERR" | sp_json_str)"
  RUNS="$RUNS${RUNS:+,}{\"name\":\"$name\",\"argv\":$(printf '%s' "$*" | sp_redact | sp_json_str),\"rc\":$rc,\"stdout\":$out,\"stderr\":$err}"
  rm -f "$o" "$e"
}

flags=(--unshare-user --unshare-pid --unshare-ipc --unshare-uts --ro-bind / / --dev /dev --proc /proc)
if [ -n "$bwrap_bin" ]; then
  run bwrap_version "$bwrap_bin" --version
  run bwrap_help "$bwrap_bin" --help
  run bwrap_real "$bwrap_bin" "${flags[@]}" -- true
  real_rc=$RUN_RC
  real_err="$RUN_ERR"
  if [ "$real_rc" -eq 0 ]; then
    run bwrap_unshare_net "$bwrap_bin" "${flags[@]}" --unshare-net -- true
  fi
  run bwrap_nested "$bwrap_bin" "${flags[@]}" -- "$bwrap_bin" "${flags[@]}" -- true
else
  real_rc=127
  real_err="bwrap absent"
fi

if sp_have systemd-run; then
  run systemd_probe systemd-run --user --wait --quiet -p ProtectSystem=strict -p PrivateTmp=yes true
  host_pts="$(find /dev/pts -mindepth 1 -maxdepth 1 2>/dev/null | wc -l | tr -d ' ')"
  run systemd_private_devices_pts systemd-run --user --wait --pipe -p PrivateDevices=yes sh -c 'find /dev/pts -mindepth 1 -maxdepth 1 2>/dev/null | wc -l'
  pts_note="host /dev/pts entries: $host_pts (compare with the stdout of systemd_private_devices_pts: equal and >1 means PrivateDevices=yes still exposes host ptys)"
else
  pts_note="systemd-run absent: not captured"
fi

max_ns="$(rd /proc/sys/user/max_user_namespaces)"
clone="$(rd /proc/sys/kernel/unprivileged_userns_clone)"
restrict="$(rd /proc/sys/kernel/apparmor_restrict_unprivileged_userns)"
aa_enabled="$(rd /sys/module/apparmor/parameters/enabled)"
aa_current="$(rd /proc/self/attr/apparmor/current)"
etc_prof=false
[ -e /etc/apparmor.d/bwrap-userns-restrict ] && etc_prof=true
extra_prof=false
[ -e /usr/share/apparmor/extra-profiles/bwrap-userns-restrict ] && extra_prof=true
setuid=false
[ -n "$bwrap_bin" ] && [ -u "$bwrap_bin" ] && setuid=true

# Suggested mapping (a human confirms; unreproduced or ambiguous => Unknown).
suggest="Unknown"
if [ -z "$bwrap_bin" ]; then
  suggest="Unknown(bwrap-absent)"
elif [ "$real_rc" -eq 0 ]; then
  suggest="Available"
  [ "$setuid" = true ] && suggest="SetuidBwrap"
elif [ "$max_ns" = 0 ]; then
  suggest="DisabledSysctl(max_user_namespaces)"
elif [ "$clone" = 0 ]; then
  suggest="DisabledSysctl(unprivileged_userns_clone)"
elif [ "$restrict" = 1 ] && printf '%s' "$real_err" | grep -qi 'uid map\|Permission denied'; then
  if [ "$etc_prof" = true ]; then
    suggest="AppArmorRestricted{LoadedForPath}"
  elif [ "$extra_prof" = true ]; then
    suggest="AppArmorRestricted{ExtraAvailableNotLoaded}"
  else
    suggest="AppArmorRestricted{Absent}"
  fi
elif printf '%s' "$real_err" | grep -qi 'No permissions to create new namespace'; then
  suggest="Denied(NoPermissionNs)"
elif printf '%s' "$real_err" | grep -qi 'uid map'; then
  suggest="Denied(UidMapDenied)"
elif printf '%s' "$real_err" | grep -qi 'RTM_NEWADDR'; then
  suggest="Denied(RtmNewaddr)"
else
  suggest="Denied(Other)"
fi

osname="$(sed -n 's/^PRETTY_NAME="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' /etc/os-release 2>/dev/null | head -1)"
{
  printf '{\n'
  printf '"case":%s,\n' "$(printf '%s' "$case_name" | sp_json_str)"
  printf '"distro":%s,\n' "$(printf '%s' "${osname:-unknown}" | sp_json_str)"
  printf '"kernel":%s,\n' "$(uname -r | sp_json_str)"
  printf '"bwrap":{"path":%s,"setuid":%s},\n' "$(printf '%s' "${bwrap_bin:-none}" | sp_json_str)" "$setuid"
  printf '"reads":{"max_user_namespaces":"%s","unprivileged_userns_clone":"%s","apparmor_restrict_unprivileged_userns":"%s","apparmor_module_enabled":"%s","apparmor_current":%s,"etc_bwrap_userns_restrict":%s,"extra_profile_available":%s},\n' \
    "$max_ns" "$clone" "$restrict" "$aa_enabled" "$(printf '%s' "$aa_current" | sp_json_str)" "$etc_prof" "$extra_prof"
  printf '"ping_group_range":%s,\n' "$(rd /proc/sys/net/ipv4/ping_group_range | sp_json_str)"
  printf '"private_devices_pts_note":%s,\n' "$(printf '%s' "$pts_note" | sp_json_str)"
  printf '"runs":[%s],\n' "$RUNS"
  printf '"suggested_status":%s,\n' "$(printf '%s' "$suggest" | sp_json_str)"
  printf '"status":"TODO: human-assigned UsernsStatus (or unreproducible: Unknown, treated unavailable)"\n'
  printf '}\n'
} >"$fx"

sp_kv fixture "$fx"
sp_kv suggested_status "$suggest"
sp_kv real_run_rc "$real_rc"
sp_kv real_run_stderr "$(printf '%s' "$real_err" | head -c 400)"
sp_verdict "each fixture maps to exactly one UsernsStatus; ambiguous or unreproduced => Unknown (treated unavailable)" \
  "$case_name suggested: $suggest (confirm by hand; review the JSON for secrets before committing)"
