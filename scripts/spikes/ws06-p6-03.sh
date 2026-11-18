#!/usr/bin/env bash
# WS06-P6-03 — the nested-virtualisation SPIKE only: does WSL2 boot inside the
# Windows 11 KVM guest? (The VM upgrade / `just test-windows-sandbox` runner work in
# that item is implementation, not a spike, and is not in this script.)
# Usage: ws06-p6-03.sh OUTDIR [DISTRO]
#   DISTRO  the WSL distro installed in the guest (default Ubuntu)
# HOST part always runs (read-only): kernel, CPU vendor, KVM nested flag, the running
# QEMU command line. GUEST part runs only when the VM is up and provisioned (the existing
# scripts/windows-vm.sh `up`, started WITH the nested CPU flag and THEGN_WINVM_WSL=1 as
# the procedure page describes); it ssh-es in with the VM's own ssh_config. This script
# never starts, provisions or destroys the VM.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS06-P6-03
sp_init "$item" "${1:-}"
distro="${2:-Ubuntu}"
[ "$(uname -s)" = Linux ] || sp_die "run on the Linux host that runs the VM"
sp_need ssh

vendor="$(sed -n 's/^vendor_id[[:space:]]*: //p' /proc/cpuinfo | head -n1)"
sp_kv cpu_vendor "$vendor"
sp_kv host_kernel "$(uname -r)"
nested="unknown"
for f in /sys/module/kvm_intel/parameters/nested /sys/module/kvm_amd/parameters/nested; do
  [ -r "$f" ] && nested="$(cat "$f") ($f)"
done
sp_kv kvm_nested "$nested"
sp_kv dev_kvm "$(find /dev/kvm -maxdepth 0 -printf '%M %u:%g\n' 2>/dev/null || echo absent)"
sp_cap qemu-cmdline 10 sh -c "ps -eo args | grep -i 'qemu.*windows-11' | grep -v grep | sed 's/-uuid [^ ]*//'"

state="${THEGN_WINVM_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/thegn-dev/windows-vm}"
cfg="$state/ssh_config"
if [ ! -r "$cfg" ]; then
  sp_note "GUEST part skipped: $cfg not found (VM never set up). Host facts above are still valid evidence."
  sp_verdict "WSL2 boots in the guest => WSL/seal/engine checks run in the VM; otherwise on the owner's Windows device" "not computable: guest not reachable"
  exit 0
fi

g() { ssh -F "$cfg" -o BatchMode=yes -o ConnectTimeout=10 thegn-winvm "$@"; }
if ! g true 2>/dev/null; then
  sp_note "GUEST part skipped: ssh to thegn-winvm failed (VM down?). Start it with scripts/windows-vm.sh up, then re-run."
  sp_verdict "WSL2 boots in the guest => WSL/seal/engine checks run in the VM; otherwise on the owner's Windows device" "not computable: guest not reachable"
  exit 0
fi

ps1() { g powershell -NoProfile -NonInteractive -Command "$1"; }
sp_cap g-build-ubr 60 g powershell -NoProfile -NonInteractive -Command "[System.Environment]::OSVersion.Version.Build; (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').UBR"
sp_cap g-hypervisor 60 g powershell -NoProfile -NonInteractive -Command "(Get-CimInstance Win32_ComputerSystem).HypervisorPresent; Get-CimInstance Win32_Processor | Select-Object VirtualizationFirmwareEnabled,SecondLevelAddressTranslationExtensions | Format-List"
sp_cap g-wsl-version 60 g wsl --version
sp_cap g-wsl-list 60 g wsl -l -v
sp_cap g-wsl-uname 120 g wsl -d "$distro" -e uname -r
uname_rc=$SP_RC
sp_cap g-wsl-uptime 60 g wsl -d "$distro" -e sh -c 'cat /proc/uptime; free -m | head -n2'
hv="$(tr -d '\r' <"$SP_OUT/$item/g-hypervisor.out" | head -n1)"
sp_kv hypervisor_present "$hv"
if [ "$uname_rc" -eq 0 ] && [ -s "$SP_OUT/$item/g-wsl-uname.out" ]; then
  v="WSL2 BOOTS in the guest (uname -r: $(tr -d '\r\0' <"$SP_OUT/$item/g-wsl-uname.out" | head -n1)): WS06-P2-00, P2-10 and P3-04 run in the VM (owner's device as second confirmation where a physical console is needed)"
else
  v="WSL2 does NOT run in the guest (rc=$uname_rc, hypervisor_present=$hv): those three items run on the owner's Windows device; the VM still serves WS06-P0-05 and the job-limit/MXC checks"
fi
sp_verdict "WSL2 boots and runs uname -r => WSL checks run in the VM; otherwise on the owner's device; never mark WSL checks done from a Linux-only substitute" "$v"
