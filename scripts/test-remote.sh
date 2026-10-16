#!/usr/bin/env bash
# Run this worktree's test gate on another machine over ssh.
#
#   scripts/test-remote.sh <ssh-host> [crate [test-filter...]]
#   just test-remote host=<ssh-host> [crate [test-filter...]]
#
# Why this exists: Linux is the only platform this repo can gate locally. The
# windows-gnu cross-check proves code compiles for Windows, and nothing proves
# it behaves there or on macOS short of running it there. A Mac with nix, or a
# Windows box (see scripts/windows-vm.sh for a local VM), is reachable over ssh
# far more often than a CI runner is affordable, so this ships the worktree to
# any ssh host and runs the repo's own gate on it.
#
# What it does, in order:
#   1. tars the worktree as git sees it — tracked files plus untracked-but-not-
#      ignored ones, so uncommitted edits are tested and target/ never is;
#   2. replaces <remote-dir>/src with it, and commits a throwaway snapshot there
#      (build.rs and some tests want a HEAD; the snapshot is not your history);
#   3. runs the gate in src with CARGO_TARGET_DIR=<remote-dir>/target, streaming
#      output, and exits with the remote gate's own status;
#   4. deletes <remote-dir>/src. target/ is KEPT so the next run warm-rebuilds
#      (set THEGN_REMOTE_CLEAN=1 to delete the whole <remote-dir> as well).
#
# Gate, unless THEGN_REMOTE_CMD overrides it:
#   unix host:    nix develop --command just test
#                 with a crate: nix develop --command cargo nextest run -p <crate> [filter...]
#   windows host: cargo test --workspace --locked
#                 with a crate: cargo test -p <crate> --locked [filter...]
# The unix gate needs nix with flakes on the remote (it is the same dev shell
# CI uses — see CONTRIBUTING "Testing on other platforms"); the windows gate
# needs rustup's msvc toolchain, Visual Studio Build Tools and git on PATH.
#
# Environment:
#   THEGN_REMOTE_DIR         remote work dir. Default: .cache/thegn-remote/<name>
#                            (unix) or tgr/<name> (windows, short for MAX_PATH),
#                            relative to the remote login directory. Plain path
#                            characters only: it is spliced into remote shells.
#   THEGN_REMOTE_CMD         replaces the gate. A POSIX sh snippet on unix, a
#                            PowerShell snippet on windows; runs in src.
#   THEGN_REMOTE_OS          unix | windows. Default: probed (`echo %OS%`).
#   THEGN_REMOTE_SSH_CONFIG  ssh config file to use instead of ~/.ssh/config
#                            (`ssh -F`); the Windows VM recipes pass their own.
#   THEGN_REMOTE_SSH         ssh binary (default: ssh). Exists for the hermetic
#                            self-test, which substitutes a local shim.
#   THEGN_REMOTE_CLEAN=1     also delete <remote-dir> (incl. target/) afterwards.
#
# No credentials live here: authentication is whatever your ssh config says.
set -euo pipefail

usage() {
  sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

[ $# -ge 1 ] || usage
host=$1
shift
# `just test-remote host=mymac` passes the literal `host=mymac` (just recipe
# parameters are positional), so accept both spellings.
host=${host#host=}
[ -n "$host" ] || usage

repo=$(git rev-parse --show-toplevel)
cd "$repo"

ssh_bin=${THEGN_REMOTE_SSH:-ssh}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/thegn-remote.XXXXXX")

# One authenticated connection for every step (sync, run, cleanup): a
# hardware-key or password login would otherwise prompt four times. The socket
# lives in our temp dir, so it cannot collide with the user's own multiplexing.
ssh_opts=(-o ControlMaster=auto -o "ControlPath=$tmp/cm-%C" -o ControlPersist=120 -o BatchMode=no)
if [ -n "${THEGN_REMOTE_SSH_CONFIG:-}" ]; then
  ssh_opts=(-F "$THEGN_REMOTE_SSH_CONFIG" "${ssh_opts[@]}")
fi

rssh() { "$ssh_bin" "${ssh_opts[@]}" "$host" "$@"; }

remote_os=${THEGN_REMOTE_OS:-}
remote_dir=""
cleanup_cmd=""

# shellcheck disable=SC2329 # invoked by the EXIT trap below
finish() {
  status=$?
  trap - EXIT INT TERM
  if [ -n "$cleanup_cmd" ]; then
    # best-effort: a failed cleanup must not replace the gate's verdict.
    rssh "$cleanup_cmd" >/dev/null 2>&1 || echo "test-remote: cleanup on $host failed (left $remote_dir)" >&2
  fi
  "$ssh_bin" "${ssh_opts[@]}" -O exit "$host" >/dev/null 2>&1 || true
  rm -rf "$tmp"
  exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ -z "$remote_os" ]; then
  # cmd.exe (the Windows OpenSSH default shell) expands %OS% to Windows_NT; a
  # POSIX shell prints it verbatim. PowerShell as the default shell would also
  # print it verbatim — that configuration needs THEGN_REMOTE_OS=windows.
  probe=$(rssh 'echo %OS%' </dev/null | tr -d '\r') || {
    echo "test-remote: cannot reach $host over ssh" >&2
    exit 255
  }
  case "$probe" in
  Windows_NT) remote_os=windows ;;
  *) remote_os=unix ;;
  esac
fi
case "$remote_os" in
unix | windows) ;;
*)
  echo "test-remote: THEGN_REMOTE_OS must be unix or windows, got '$remote_os'" >&2
  exit 2
  ;;
esac

name=$(basename "$repo" | tr -c 'A-Za-z0-9._\n-' '_')
if [ "$remote_os" = windows ]; then
  remote_dir=${THEGN_REMOTE_DIR:-tgr/$name}
else
  remote_dir=${THEGN_REMOTE_DIR:-.cache/thegn-remote/$name}
fi
remote_dir=${remote_dir#\~/}
case "$remote_dir" in
'' | *[!A-Za-z0-9._/:-]*)
  echo "test-remote: THEGN_REMOTE_DIR may only contain [A-Za-z0-9._/:-], got '$remote_dir'" >&2
  exit 2
  ;;
esac

# --- 1. pack ----------------------------------------------------------------
# `ls-files --cached` still lists a tracked file deleted in the working tree;
# tar would abort on it, so keep only paths that exist.
git ls-files -z --cached --others --exclude-standard |
  while IFS= read -r -d '' f; do
    if [ -e "$f" ] || [ -L "$f" ]; then printf '%s\0' "$f"; fi
  done >"$tmp/files"
count=$(tr -cd '\0' <"$tmp/files" | wc -c | tr -d ' ')
# COPYFILE_DISABLE: a macOS host's bsdtar would otherwise add ._ resource forks.
COPYFILE_DISABLE=1 tar -czf "$tmp/src.tgz" --null -T "$tmp/files"
head_sha=$(git rev-parse --short HEAD)
dirty=""
git diff --quiet HEAD -- 2>/dev/null || dirty="+dirty"
echo "test-remote: $count files ($(du -k "$tmp/src.tgz" | cut -f1) KiB) of $head_sha$dirty -> $host:$remote_dir ($remote_os)" >&2

# --- 2-4. ship, run, clean ----------------------------------------------------
gate_args=("$@")
snapshot_msg="thegn test-remote snapshot of $head_sha$dirty"

if [ "$remote_os" = unix ]; then
  if [ -n "${THEGN_REMOTE_CMD:-}" ]; then
    gate=$THEGN_REMOTE_CMD
  elif [ ${#gate_args[@]} -eq 0 ]; then
    gate='nix develop --command just test'
  else
    # printf %q output is bash syntax; the run script below is executed by
    # bash on the remote for exactly this reason.
    gate="nix develop --command cargo nextest run -p$(printf ' %q' "${gate_args[@]}")"
  fi
  if [ "${THEGN_REMOTE_CLEAN:-0}" = 1 ]; then
    cleanup_cmd="rm -rf '$remote_dir'"
  else
    cleanup_cmd="rm -rf '$remote_dir/src' '$remote_dir/run.sh'"
  fi
  rssh "mkdir -p '$remote_dir' && rm -rf '$remote_dir/src' && mkdir '$remote_dir/src' && tar -xzf - -C '$remote_dir/src'" <"$tmp/src.tgz"
  # Written as a file and run by path rather than fed on stdin: a test that
  # reads stdin would otherwise consume the rest of the script.
  cat >"$tmp/run.sh" <<EOF
set -eu
# Non-interactive ssh does not read the login profile that puts nix on PATH on
# macOS (the Determinate installer hooks /etc/zshrc); source it explicitly.
if ! command -v nix >/dev/null 2>&1 && [ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ]; then
  . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
fi
cd '$remote_dir/src'
export CARGO_TARGET_DIR="\$(cd .. && pwd)/target"
if command -v git >/dev/null 2>&1; then
  git init -q && git add -A &&
    git -c user.name=thegn-remote -c user.email=thegn-remote@localhost -c commit.gpgsign=false \\
      commit -q --no-verify -m '$snapshot_msg'
fi
echo "test-remote: running on \$(uname -sm)" >&2
$gate
EOF
  rssh "cat > '$remote_dir/run.sh'" <"$tmp/run.sh"
  printf 'test-remote: gate:\n%s\n' "$gate" >&2
  tflag=()
  if [ -t 0 ] && [ -t 1 ]; then tflag=(-t); fi
  set +e
  "$ssh_bin" "${ssh_opts[@]}" "${tflag[@]}" "$host" "bash '$remote_dir/run.sh'"
  rc=$?
  set -e
else
  # Windows: the OpenSSH default shell is cmd.exe. Everything non-trivial runs
  # as PowerShell via -EncodedCommand (base64 UTF-16LE), which sidesteps cmd's
  # quoting entirely; only the tar extraction runs straight under cmd, because
  # PowerShell 5 re-encodes binary data piped to a native command's stdin.
  winpath=${remote_dir//\//\\}
  ps_cmd() {
    local enc
    enc=$(printf '%s' "$1" | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')
    printf 'powershell -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand %s' "$enc"
  }
  if [ -n "${THEGN_REMOTE_CMD:-}" ]; then
    gate=$THEGN_REMOTE_CMD
  elif [ ${#gate_args[@]} -eq 0 ]; then
    gate='cargo test --workspace --locked'
  else
    gate="cargo test -p ${gate_args[0]} --locked"
    for a in "${gate_args[@]:1}"; do gate+=" '${a//\'/\'\'}'"; done
  fi
  if [ "${THEGN_REMOTE_CLEAN:-0}" = 1 ]; then
    cleanup_cmd=$(ps_cmd "Remove-Item -Recurse -Force -LiteralPath '$winpath' -ErrorAction SilentlyContinue")
  else
    cleanup_cmd=$(ps_cmd "Remove-Item -Recurse -Force -LiteralPath '$winpath\\src' -ErrorAction SilentlyContinue")
  fi
  rssh "$(ps_cmd "\$ErrorActionPreference='Stop'; Remove-Item -Recurse -Force -LiteralPath '$winpath\\src' -ErrorAction SilentlyContinue; New-Item -ItemType Directory -Force -Path '$winpath\\src' | Out-Null")" </dev/null
  rssh "tar -xzf - -C $winpath\\src" <"$tmp/src.tgz"
  run=$(
    cat <<EOF
\$ErrorActionPreference = 'Stop'
\$env:Path = "\$env:USERPROFILE\\.cargo\\bin;\$env:Path"
Set-Location -LiteralPath '$winpath\\src'
\$env:CARGO_TARGET_DIR = (Join-Path (Resolve-Path ..).Path 'target')
if (Get-Command git -ErrorAction SilentlyContinue) {
  git init -q; git -c core.autocrlf=false add -A
  git -c user.name=thegn-remote -c user.email=thegn-remote@localhost -c commit.gpgsign=false commit -q --no-verify -m '$snapshot_msg'
}
[Console]::Error.WriteLine("test-remote: running on " + [Environment]::OSVersion.VersionString)
\$ErrorActionPreference = 'Continue'
\$global:LASTEXITCODE = 0
$gate
exit \$LASTEXITCODE
EOF
  )
  printf 'test-remote: gate:\n%s\n' "$gate" >&2
  set +e
  rssh "$(ps_cmd "$run")" </dev/null
  rc=$?
  set -e
fi

echo "test-remote: $host exited $rc" >&2
exit "$rc"
