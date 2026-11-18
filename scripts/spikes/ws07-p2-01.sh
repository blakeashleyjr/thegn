#!/usr/bin/env bash
# WS07-P2-01 — verify smolvm on a real install (16 checks). Linux host with KVM
# first, then the owner's Mac (macOS rows are separate runs; never substitute).
# Usage: SMOLVM_BIN=smolvm ws07-p2-01.sh OUTDIR
#   SMOLVM_BIN   the installed smolvm (default: smolvm on PATH). Install it by hand
#                for the spike only and record how and its checksum; thegn never
#                downloads wrapper binaries. Releases are unsigned.
# Runs in an EMPTY scratch dir (so no repo Smolfile is discovered), with a throwaway
# machine prefix tgspike-<pid>. Every machine is deleted and any listener killed by
# trap. Never starts `smolvm serve`. Mounts only the scratch dir: never $HOME.
# Verdict per check is auto-computed where deterministic, else REVIEW (a human reads
# the capture). The flag spellings are the source-read (1.35.0) forms; a flag that
# differs on the real install is itself a finding ("differs").
# shellcheck disable=SC2016,SC2012,SC2009 # single quotes expand in the guest; ls -ln owner and ps greps are the evidence
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS07-P2-01
sp_init "$item" "${1:-}"
SM="${SMOLVM_BIN:-smolvm}"
sp_need "$SM" python3 grep ps
unset SMOLVM_MACHINE_NAME
case "$(uname -s)" in Linux) [ -r /dev/kvm ] && [ -w /dev/kvm ] || sp_die "/dev/kvm not usable on this Linux host (record: not installable/runnable)" ;; esac

W="$(mktemp -d "${TMPDIR:-/tmp}/tgspike-smol.XXXXXX")"
W="$(cd "$W" && pwd -P)"
sp_on_exit "rm -rf '$W'"
cd "$W"
mkdir -p "$W/vol"
P="tgspike-$$"
MACHINES=""
mk() { MACHINES="$MACHINES $1"; }
sp_on_exit 'for m in $MACHINES; do "$SM" machine delete --name "$m" --force; done'

sp_kv smolvm_version "$("$SM" --version 2>&1 | head -n1)"
sp_kv smolvm_path "$(command -v "$SM")"
sp_kv smolvm_sha256 "$(shasum -a 256 "$(command -v "$SM")" 2>/dev/null | cut -d' ' -f1 || sha256sum "$(command -v "$SM")" | cut -d' ' -f1)"
sp_kv kernel "$(uname -srm)"

res() { sp_note "CHECK $1: $2 - $3"; }
V="$W/vol"

# --- 14 default image (create with no --image) and 1 lifecycle
N1="$P-a"
mk "$N1"
sp_cap c14-create-default 300 "$SM" machine create -n "$N1" --label thegn.owner=spike -v "$V:$V"
sp_cap c3-status-stopped 30 "$SM" machine status --name "$N1" --json
sp_cap c3-status-absent 30 "$SM" machine status --name "$P-absent" --json
absent_rc=$SP_RC
sp_cap c1-start 300 "$SM" machine start --name "$N1"
sp_cap c3-status-running 30 "$SM" machine status --name "$N1" --json
sp_cap c1-exec-true 60 "$SM" machine exec --name "$N1" -- true
sp_cap c1-exec-false 60 "$SM" machine exec --name "$N1" -- false
rcf=$SP_RC
sp_cap c1-exec-exit7 60 "$SM" machine exec --name "$N1" -- sh -c 'exit 7'
rc7=$SP_RC
sp_cap c1-exec-missing 60 "$SM" machine exec --name "$P-absent" -- true
res 1 "REVIEW" "exec false rc=$rcf, exit 7 rc=$rc7 (expect propagated), missing-machine rc=$SP_RC, status-absent rc=$absent_rc. -i -t (PTY) exec is MANUAL: run it in a terminal and paste the result"
sp_cap c14-default-image 30 "$SM" machine ls --json
res 14 "REVIEW" "default image/size: read c14-default-image.out and c14-create-default"

# --- 2 ls --json shape + label round trip
if grep -q 'thegn.owner' "$SP_OUT/$item/c14-default-image.out"; then
  res 2 "PASS" "label thegn.owner round-trips in ls --json; check fields name,state,pid,labels,network by eye"
else
  res 2 "FAIL" "label thegn.owner not visible in ls --json (differs: see c14-default-image.out)"
fi

# --- 4 path-preserving volume, 6 -w, 5 ownership
sp_cap c4-pwd 60 "$SM" machine exec --name "$N1" -w "$V" -- sh -c 'pwd; echo hi > probe; cat probe'
first="$(head -n1 "$SP_OUT/$item/c4-pwd.out" | tr -d '\r')"
sp_redact_cmp() { printf '%s' "$1" | sp_redact; }
if [ -f "$V/probe" ] && [ "$(cat "$V/probe")" = hi ]; then hostsees=yes; else hostsees=no; fi
if [ "$(sp_redact_cmp "$first")" = "$(sp_redact_cmp "$V")" ] && [ "$hostsees" = yes ]; then
  res 4 "PASS" "guest pwd == host path and the host sees the write"
else
  res 4 "FAIL" "NOT path-preserving (guest pwd='$(sp_redact_cmp "$first")', host sees write=$hostsees): smolvm is NOT shipped (WS07-P2-03..P2-10 stop)"
fi
sp_cap c6-workdir 60 "$SM" machine exec --name "$N1" -w "$V" -- pwd
res 6 "REVIEW" "-w honoured for exec: c6-workdir.out; check create -w by hand"
sp_cap c5-user 60 "$SM" machine exec --name "$N1" -u "$(id -u):$(id -g)" -w "$V" -- sh -c 'echo a > u.txt'
sp_cap c5-root 60 "$SM" machine exec --name "$N1" -w "$V" -- sh -c 'echo b > r.txt'
uown="$(ls -ln "$V/u.txt" 2>/dev/null | awk '{print $3}')"
rown="$(ls -ln "$V/r.txt" 2>/dev/null | awk '{print $3}')"
res 5 "REVIEW" "host uid=$(id -u): file written with -u uid:gid is owned by uid ${uown:-?}; written as guest root is owned by uid ${rown:-?} (root-owned/unwritable => template must always pass -u uid:gid)"

# --- 7 network off (no --net), 8 --block-private, 9 --allow-host
sp_cap c7-no-net 60 "$SM" machine exec --name "$N1" -- sh -c 'timeout 5 nc -z 1.1.1.1 443; echo nc_rc=$?'
if grep -q 'nc_rc=0' "$SP_OUT/$item/c7-no-net.out"; then res 7 "FAIL" "egress worked with no --net"; else res 7 "PASS" "no egress without --net (confirm DNS by hand: nslookup example.com)"; fi

lan="$(python3 -c 'import socket; s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.connect(("192.0.2.1", 9)); print(s.getsockname()[0])' 2>/dev/null || true)"
cat >"$W/listen.py" <<'PY'
import socket, sys
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", int(sys.argv[1])))
s.listen(8)
while True:
    c, _ = s.accept()
    c.close()
PY
python3 "$W/listen.py" 18097 &
lpid=$!
sp_on_exit "kill $lpid"
sleep 1
N2="$P-b"
mk "$N2"
sp_cap c8-create 300 "$SM" machine create -n "$N2" --net --block-private
sp_cap c8-start 300 "$SM" machine start --name "$N2"
sp_cap c8-public 60 "$SM" machine exec --name "$N2" -- sh -c 'timeout 8 nc -z 1.1.1.1 443; echo public_rc=$?'
sp_cap c8-lan 60 "$SM" machine exec --name "$N2" -- sh -c "timeout 8 nc -z ${lan:-192.168.0.1} 18097; echo lan_rc=\$?"
sp_cap c8-gateways 90 "$SM" machine exec --name "$N2" -- sh -c 'for g in 10.0.2.2 192.168.127.1 172.16.0.1; do timeout 4 nc -z $g 18097; echo gw_${g}_rc=$?; done'
res 8 "REVIEW" "public egress with --net --block-private, LAN ${lan:-unknown}:18097 and gateway reachability: read c8-*.out (host listener on 0.0.0.0:18097). Loopback/gateway reach by hand."

N3="$P-c"
mk "$N3"
sp_cap c9-create 300 "$SM" machine create -n "$N3" --net --allow-host example.com
sp_cap c9-start 300 "$SM" machine start --name "$N3"
sp_cap c9-allowed 60 "$SM" machine exec --name "$N3" -- sh -c 'timeout 8 nc -z example.com 443; echo allowed_rc=$?'
sp_cap c9-other 60 "$SM" machine exec --name "$N3" -- sh -c 'timeout 8 nc -z 1.1.1.1 443; echo other_rc=$?'
if grep -q 'allowed_rc=0' "$SP_OUT/$item/c9-allowed.out" && ! grep -q 'other_rc=0' "$SP_OUT/$item/c9-other.out"; then
  res 9 "PASS" "only the allowed host is reachable (resolved at VM start: change the IP of example.com manually to confirm it is not followed)"
else
  res 9 "FAIL" "allow-host pinning not as described (native:hostname-at-start downgraded in the manifest)"
fi

# --- 10 secret-env
SECRET="tgspike-secret-$$-value"
sp_cap c10-exec 60 env K="$SECRET" "$SM" machine exec --name "$N1" --secret-env K=K -- sh -c 'echo guest_sees=$K'
guest_ok=no
grep -q "guest_sees=$SECRET" "$SP_OUT/$item/c10-exec.out" && guest_ok=yes
leak=no
for d in "$HOME/.local/share/smolvm" "$HOME/Library/Application Support/smolvm" "${XDG_DATA_HOME:-/nonexistent}/smolvm" "$HOME/.smolvm"; do
  [ -d "$d" ] && sp_timeout 90 grep -rqF "$SECRET" "$d" 2>/dev/null && leak="yes in $d"
done
ps -eo args | grep -F "$SECRET" | grep -v grep >/dev/null 2>&1 && leak="$leak; visible in ps argv"
if [ "$guest_ok" = yes ] && [ "$leak" = no ]; then res 10 "PASS" "guest sees it; not in machine dirs searched; not in ps argv"; else res 10 "FAIL" "guest_sees=$guest_ok persisted/visible=$leak (manifest: secrets = unsupported)"; fi

# --- 11 hostile env
sp_cap c11-explicit 60 env SMOLVM_MACHINE_NAME=other "$SM" machine exec --name "$N1" -- hostname
rc11=$SP_RC
sp_cap c11-implicit 60 env SMOLVM_MACHINE_NAME="$N1" "$SM" machine exec -- hostname
if [ "$rc11" -eq 0 ]; then res 11 "PASS" "explicit --name wins over SMOLVM_MACHINE_NAME; c11-implicit shows a name-less call acts on the env machine (wrapper must clear env)"; else res 11 "FAIL" "explicit --name did NOT win (rc=$rc11): wrapper must env_clear; blocker for P2-03 env_allow"; fi

# --- 12 fork / mounts / pause (machine must be stopped before --branchable start)
sp_cap c12-stop 120 "$SM" machine stop --name "$N1"
sp_cap c12-start-branchable 300 "$SM" machine start --name "$N1" --branchable
NB="$P-a-child"
mk "$NB"
sp_cap c12-branch 300 "$SM" machine branch --from "$N1" --name "$NB"
sp_cap c12-ident-parent 60 "$SM" machine exec --name "$N1" -- sh -c 'hostname; cat /etc/machine-id 2>/dev/null; cat /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null'
sp_cap c12-ident-child 60 "$SM" machine exec --name "$NB" -- sh -c 'hostname; cat /etc/machine-id 2>/dev/null; cat /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null'
mkdir -p "$W/vol2" "$W/vol3"
sp_cap c12-mount-add-two 60 "$SM" machine mount add --name "$N1" -v "$W/vol2:$W/vol2" -v "$W/vol3:$W/vol3"
sp_cap c12-mount-remove 60 "$SM" machine mount remove --name "$N1" -t "$W/vol2"
sp_cap c12-pause 60 "$SM" machine pause --name "$N1"
sp_cap c12-resume 60 "$SM" machine resume --name "$N1"
if cmp -s "$SP_OUT/$item/c12-ident-parent.out" "$SP_OUT/$item/c12-ident-child.out"; then ident="IDENTICAL (not re-minted)"; else ident="differ (re-minted)"; fi
res 12 "REVIEW" "branch identity parent vs child: $ident; repeating -v in one mount add rc: see c12-mount-add-two (if non-zero, P5-03 issues one mount add per mount)"

# --- 13 exec-session close
"$SM" machine exec --name "$N1" -- sleep 300 >/dev/null 2>&1 &
cpid=$!
sleep 3
kill -9 "$cpid" 2>/dev/null || true
wait "$cpid" 2>/dev/null || true
sleep 2
sp_cap c13-survivors 60 "$SM" machine exec --name "$N1" -- sh -c 'ps | grep -c "[s]leep 300"'
n13="$(tr -dc '0-9' <"$SP_OUT/$item/c13-survivors.out")"
if [ "${n13:-0}" -gt 0 ]; then res 13 "PASS(observation)" "guest process SURVIVES a killed exec client: kill_tree stays best-effort-pgid; teardown relies on stop/delete"; else res 13 "PASS(observation)" "killing the exec client ended the guest process"; fi

# --- 15 read-only / discard rootfs options
sp_cap c15-create-help 30 "$SM" machine create --help
sp_cap c15-start-help 30 "$SM" machine start --help
if grep -qiE 'read-only|readonly|ephemeral|discard|overlay|rootfs' "$SP_OUT/$item/c15-create-help.out" "$SP_OUT/$item/c15-start-help.out"; then
  res 15 "REVIEW" "help text mentions a candidate option: try it; do writes outside mounts persist across stop/start? (yes => read_only_root unsupported)"
else
  res 15 "REVIEW" "no read-only/discard option in --help: record read_only_root = unsupported unless the docs show one (O23 default: sealed refuses smolvm)"
fi

# --- 16 resident processes
sleep 60
procs() { ps -eo pid=,ppid=,rss=,args= | grep -i smol | grep -v -e grep -e "$0" | sp_redact; }
procs >"$SP_OUT/$item/c16-running.txt"
"$SM" machine stop --name "$N1" >/dev/null 2>&1 || true
"$SM" machine stop --name "$N2" >/dev/null 2>&1 || true
"$SM" machine stop --name "$N3" >/dev/null 2>&1 || true
"$SM" machine stop --name "$NB" >/dev/null 2>&1 || true
procs >"$SP_OUT/$item/c16-after-stop.txt"
for m in $MACHINES; do "$SM" machine delete --name "$m" --force >/dev/null 2>&1 || true; done
MACHINES=""
procs >"$SP_OUT/$item/c16-after-delete.txt"
res 16 "REVIEW" "resident count: running=$(wc -l <"$SP_OUT/$item/c16-running.txt" | tr -d ' ') after-stop=$(wc -l <"$SP_OUT/$item/c16-after-stop.txt" | tr -d ' ') after-delete=$(wc -l <"$SP_OUT/$item/c16-after-delete.txt" | tr -d ' ') (running count covers 4 machines: divide per machine; after-delete must be 0). RSS in the c16-*.txt files"

sp_note ""
sp_note "## processes left behind (must be empty)"
procs >>"$SP_EVID" || true
fails="$(grep -c '^CHECK [0-9]*: FAIL' "$SP_EVID" || true)"
c4="$(grep '^CHECK 4:' "$SP_EVID" | head -n1)"
sp_verdict "check 4 fails => smolvm is NOT shipped; any verb/flag difference => correct plan 3.4 before P2-03; 15 => read_only_root slot/unsupported; 16 => resident count; measured version = version_req lower bound" \
  "auto FAIL count=$fails; $c4; REVIEW rows need a human read. Record smolvm $(sed -n 's/^smolvm_version: //p' "$SP_EVID" | head -n1) on $(uname -s)."
