#!/usr/bin/env bash
# WS05-P0-06 — Seatbelt primitive cases (19 ids). Run ON A MAC, from an INTERACTIVE
# terminal (the PTY cases need a controlling tty), on macOS 26.x Apple silicon AND
# on macOS 15; run the V-VIO-01 admin variant as an admin user and again as a
# non-admin user (the script records which).
# Usage: ws05-p0-06.sh OUTDIR
# Creates only: a scratch dir under $TMPDIR, /tmp/tgspike-u-<pid>, listeners on
# TCP 18098, a defaults domain thegn.test.secret, $HOME/.tgspike-path-<pid>, a
# DARWIN_USER_CACHE_DIR/tgspike-layout dir, and a tmux server socket 'tgspike'.
# All removed by trap. Evidence: OUTDIR/WS05-P0-06.evidence.jsonl (the WS05-P0-01
# line format; reuses the shared case runner, no second runner).
# The SBPL below is a hand-written starting point: each line records the argv.
# Profiles use (allow default) plus targeted denies so one probe tests one primitive;
# a refined deny-default profile is the later WS05-P1 generator's job.
# WS05 depends on WS05-P0-01 for the runner semantics; this script embeds the same
# JSON-lines contract via scripts/spikes/lib-cases.sh.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"
# shellcheck source=lib-cases.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib-cases.sh"

[ "$(uname -s)" = Darwin ] || {
  echo "macOS only" >&2
  exit 2
}
SBX=/usr/bin/sandbox-exec
[ -x "$SBX" ] || sp_die "$SBX missing (V-SB-01 fails: backend reports Absent)"
sc_init WS05-P0-06 "${1:-}"

T="$(mktemp -d "${TMPDIR:-/tmp}/tgspike-sb.XXXXXX")"
T="$(cd "$T" && pwd -P)" # canonical: SBPL matches the resolved path
sp_on_exit "rm -rf '$T'"
U="/tmp/tgspike-u-$$" # short path: AF_UNIX SUN_LEN is 104 on macOS
mkdir -p "$U"
sp_on_exit "rm -rf '$U'"
mkdir -p "$T/granted" "$T/denied"
echo secret >"$T/denied/canary"
is_admin=no
id -Gn | tr ' ' '\n' | grep -qx admin && is_admin=yes
sc_line _env "" 0 info "" "is_admin=$is_admin macos=$SC_MACOS" "" recorded

# ---------------------------------------------------------------- V-SB
SC_NEEDS=stat sc_case V-SB-01 any stat -f '%Su %Sp %N' "$SBX"
sc_case V-SB-02-control rc0 touch "$T/denied/ctl"
sc_case V-SB-02 rcnz "$SBX" -D K="$T/denied" -p '(version 1)(allow default)(deny file-write* (subpath (param "K")))' touch "$T/denied/f"
sc_case V-SB-03 rcnz "$SBX" -p '(version 1)(bogus' touch "$T/ran-marker"
if [ -e "$T/ran-marker" ]; then sc_line V-SB-03-marker "" 0 info "" "command RAN despite a bad profile" "" fail; else sc_line V-SB-03-marker "" 0 info "" "command did not run" "" pass; fi
sc_case V-SB-04 any "$SBX" -p '(version 1)(allow default)' "$SBX" -p '(version 1)(allow default)' true
sc_case V-SB-05 rc0 "$SBX" -p '(version 1)(allow default)(allow file-map-executable (subpath "/usr/lib"))(allow file-map-executable (subpath "/opt/homebrew"))' /usr/bin/true
sc_case V-SB-05-dylib any "$SBX" -p '(version 1)(allow default)(allow file-map-executable (subpath "/usr/lib"))' python3 -c 'import sqlite3'
defaults write thegn.test.secret k v
sp_on_exit "defaults delete thegn.test.secret"
SB07='(version 1)(allow default)(deny user-preference-read (preference-domain "thegn.test.secret"))'
sc_case V-SB-07-host rc0 defaults read thegn.test.secret k
sc_case V-SB-07-sealed rcnz "$SBX" -p "$SB07" defaults read thegn.test.secret k
sc_case V-SB-07-global rc0 "$SBX" -p "$SB07" defaults read -g AppleLanguages

# ---------------------------------------------------------------- V-PTY (need a tty)
tty_path="$(tty 2>/dev/null || true)"
case "$tty_path" in
/dev/tty*) have_tty=1 ;;
*) have_tty=0 ;;
esac
PTYPROF='(version 1)(allow default)(deny file-read* file-write* file-ioctl (regex #"^/dev/ttys[0-9]+$"))(allow file-read* file-write* file-ioctl (literal (param "THEGN_TTY")))'
if [ "$have_tty" = 1 ]; then
  # shellcheck disable=SC2016 # expanded by the inner sh
  sc_case V-PTY-01 rc0 env THEGN_TTY="$tty_path" "$SBX" -D THEGN_TTY="$tty_path" -p "$PTYPROF" sh -c 'stty -a < "$THEGN_TTY" >/dev/null && echo pty-ok > "$THEGN_TTY"'
  sc_case V-PTY-04 any "$SBX" -D THEGN_TTY="$tty_path" -p "$PTYPROF" sh -c 'echo x > /dev/tty'
  sc_case V-PTY-05-script rc0 "$SBX" -D THEGN_TTY="$tty_path" -p "$PTYPROF" script -q /dev/null true
  sp_on_exit "tmux -L tgspike kill-server"
  sc_case V-PTY-05-tmux rc0 "$SBX" -D THEGN_TTY="$tty_path" -p "$PTYPROF" tmux -L tgspike new-session -d true
  other="$(find /dev -maxdepth 1 -name 'ttys[0-9]*' ! -path "$tty_path" 2>/dev/null | head -1)"
  if [ -n "$other" ]; then
    # shellcheck disable=SC2016 # expanded by the inner sh
    sc_case V-PTY-07-open rcnz env OTHER="$other" "$SBX" -D THEGN_TTY="$tty_path" -p "$PTYPROF" sh -c ': < "$OTHER"'
    sc_case V-PTY-07-evtonly rcnz "$SBX" -D THEGN_TTY="$tty_path" -p "$PTYPROF" python3 -c 'import os,sys; os.open(sys.argv[1], 0x8000)' "$other"
  else
    sc_skip V-PTY-07 "no second /dev/ttys* to attack: open another terminal tab and re-run"
  fi
else
  for c in V-PTY-01 V-PTY-04 V-PTY-05 V-PTY-07; do sc_skip "$c" "no controlling tty: run from an interactive terminal"; done
fi

# ---------------------------------------------------------------- V-PATH-03 firmlink spellings
pd="$HOME/.tgspike-path-$$"
mkdir -p "$pd"
echo p >"$pd/f"
sp_on_exit "rm -rf '$pd'"
case "$pd" in
/Users/*)
  fl="/System/Volumes/Data$pd"
  sc_case V-PATH-03-deny-users-read-firmlink rcnz "$SBX" -p "(version 1)(allow default)(deny file-read* (subpath \"$pd\"))" cat "$fl/f"
  sc_case V-PATH-03-deny-firmlink-read-users rcnz "$SBX" -p "(version 1)(allow default)(deny file-read* (subpath \"$fl\"))" cat "$pd/f"
  ;;
*) sc_skip V-PATH-03 "HOME is not under /Users: no firmlink spelling to test" ;;
esac

# ---------------------------------------------------------------- V-HOME
REAL="$T/realhome"
PRIV="$T/privhome"
mkdir -p "$REAL/.ssh" "$REAL/allowdir" "$PRIV"
echo canary >"$REAL/.ssh/canary"
echo ok >"$REAL/allowdir/file"
ln -s "$REAL/allowdir/file" "$PRIV/allowed-file"
HP="(version 1)(allow default)(deny file-read* (subpath \"$REAL\"))(allow file-read* (literal \"$REAL/allowdir/file\"))"
sc_case V-HOME-01-allowed rc0 "$SBX" -p "$HP" cat "$PRIV/allowed-file"
sc_case V-HOME-01-canary rcnz "$SBX" -p "$HP" cat "$REAL/.ssh/canary"
sc_case V-HOME-01-ssh-G any env HOME="$PRIV" "$SBX" -p "$HP" ssh -G localhost
# shellcheck disable=SC2016 # expanded by the inner sh
sc_case V-HOME-02 rc0 sh -c 'd=$(getconf DARWIN_USER_CACHE_DIR) && test -d "$d" && test -w "$d" && mkdir -p "$d/tgspike-layout" && touch "$d/tgspike-layout/x" && echo "$d"'
# shellcheck disable=SC2016 # expanded by the inner sh
sp_on_exit 'rm -rf "$(getconf DARWIN_USER_CACHE_DIR)/tgspike-layout"'

# ---------------------------------------------------------------- V-UNIX-01 / V-NET
cat >"$T/unix_server.py" <<'PY'
import os, socket, sys
p = sys.argv[1]
s = socket.socket(socket.AF_UNIX)
s.bind(p)
s.listen(8)
while True:
    c, _ = s.accept()
    c.close()
PY
cat >"$T/connect_unix.py" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(3)
s.connect(sys.argv[1])
PY
cat >"$T/tcp_server.py" <<'PY'
import socket, sys
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", int(sys.argv[1])))
s.listen(8)
while True:
    c, _ = s.accept()
    c.close()
PY
cat >"$T/connect_tcp.py" <<'PY'
import socket, sys
socket.create_connection((sys.argv[1], int(sys.argv[2])), 3)
PY
python3 "$T/unix_server.py" "$U/sock" &
upid=$!
sp_on_exit "kill $upid"
python3 "$T/tcp_server.py" 18098 &
tpid=$!
sp_on_exit "kill $tpid"
sleep 1
IPONLY='(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "*:*"))'
sc_case V-UNIX-01-iponly-blocks-unix rcnz "$SBX" -p "$IPONLY" python3 "$T/connect_unix.py" "$U/sock"
sc_case V-UNIX-01-grant-allows rc0 "$SBX" -p "$IPONLY(allow network-outbound (remote unix-socket (subpath \"$U\")))" python3 "$T/connect_unix.py" "$U/sock"
sc_case V-UNIX-01-trailing-deny-wins rcnz "$SBX" -p "$IPONLY(allow network-outbound (remote unix-socket (subpath \"$U\")))(deny network-outbound (remote unix-socket (subpath \"$U\")))" python3 "$T/connect_unix.py" "$U/sock"

NONET='(version 1)(allow default)(deny network*)'
sc_case V-NET-01-nc rcnz "$SBX" -p "$NONET" nc -z -w 3 1.1.1.1 443
sc_case V-NET-01-curl rcnz "$SBX" -p "$NONET" curl -m 5 -sS -o /dev/null https://1.1.1.1
sc_case V-NET-01-dscacheutil any "$SBX" -p "$NONET" dscacheutil -q host -a name example.com

host_ip="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || echo)"
LO='(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:18098"))'
sc_case V-NET-06-loopback rc0 "$SBX" -p "$LO" python3 "$T/connect_tcp.py" 127.0.0.1 18098
if [ -n "$host_ip" ]; then
  sc_case V-NET-06-en0 rcnz "$SBX" -p "$LO" python3 "$T/connect_tcp.py" "$host_ip" 18098
  sc_line V-NET-06-addr "" 0 info "" "non-loopback address used: $host_ip" "" recorded
else
  sc_skip V-NET-06-en0 "no en0/en1 address; run by hand against the host's LAN address"
fi

# ---------------------------------------------------------------- V-VIO-01 / V-NEST-02 / V-LOG-01
# logprobe.sh PROFILE_TEXT PREDICATE cmd... : stream the unified log for 8 s while the
# sandboxed command runs; print the number of matching lines and the first three.
cat >"$T/logprobe.sh" <<'SH'
#!/bin/sh
prof="$1"; pred="$2"; shift 2
out="$(mktemp)"
log stream --style ndjson --predicate "$pred" >"$out" 2>&1 &
lp=$!
sleep 2
/usr/bin/sandbox-exec -p "$prof" "$@" >/dev/null 2>&1
sleep 3
kill "$lp" 2>/dev/null
wait "$lp" 2>/dev/null
echo "matching_lines=$(grep -c '~X' "$out")"
grep '~X' "$out" | head -n 3
rm -f "$out"
SH
chmod +x "$T/logprobe.sh"
SC_TIMEOUT=40 sc_case V-VIO-01-ndjson-"$is_admin" any "$T/logprobe.sh" "(version 1)(allow default)(deny file-read-data (subpath \"$T/denied\") (with message \"tag~X\"))" 'eventMessage ENDSWITH "~X"' cat "$T/denied/canary"
SC_TIMEOUT=40 sc_case V-NEST-02 any "$T/logprobe.sh" '(version 1)(allow default)' 'eventMessage CONTAINS "sandbox" AND eventMessage ENDSWITH "~X"' "$SBX" -p '(version 1)(allow default)' true
LOGD='(version 1)(allow default)(deny mach-lookup)(allow mach-lookup (global-name "com.apple.logd"))'
sc_case V-LOG-01-baseline any log show --last 1m --style compact
sc_case V-LOG-01 any "$SBX" -p "$LOGD" log show --last 1m --style compact

# ---------------------------------------------------------------- summary
pass="$(grep -c '"verdict":"pass"' "$SP_EVID" || true)"
fail="$(grep -c '"verdict":"fail"' "$SP_EVID" || true)"
rec="$(grep -c '"verdict":"recorded"' "$SP_EVID" || true)"
skip="$(grep -c 'skipped' "$SP_EVID" || true)"
sc_verdict "pass=$pass fail=$fail recorded=$rec skipped=$skip. Per-case pass/fail vs the issue table, then the consumer-item branch table, is the human decision step. BLOCKERS to raise at once: any V-PTY-07* fail, any V-UNIX-01* fail. Run on macOS 15 and 26.x; V-VIO-01 as admin and non-admin (this run: is_admin=$is_admin)."
