#!/usr/bin/env bash
# 30-git-integrity.sh — git control files and host-executed worktree files are
# read-only inside a bubblewrap sandbox laid out the way thegn lays it out
# (THE-2147): writable worktree and common dir, then `--ro-bind` pins after.
#
# Passes when writes to <common>/hooks/pre-commit, <wt>/.git, config.worktree,
# .claude/settings.json and .envrc all fail with a read-only filesystem error
# while an ordinary worktree file stays writable. The repo lives in a temp dir
# outside $HOME; the bind shape is the same inside it. The systemd lowering is
# asserted on its property list by the sandbox unit tests.
# Skips (77) when bwrap or user namespaces are unavailable.
set -euo pipefail

if ! command -v bwrap >/dev/null 2>&1; then
  echo "bwrap not found on PATH" >&2
  exit 77
fi
if ! bwrap --unshare-user --unshare-pid --ro-bind / / -- true 2>/dev/null; then
  echo "bwrap cannot create a user namespace here" >&2
  exit 77
fi

root="$(mktemp -d)"
trap 'rm -rf "$root"' EXIT
common="$root/repo/.git"
admin="$common/worktrees/lane"
wt="$root/wt"
mkdir -p "$common/hooks" "$common/info" "$admin" "$wt/.claude"
: >"$common/config"
: >"$admin/config.worktree"
: >"$admin/commondir"
: >"$admin/gitdir"
echo "gitdir: $admin" >"$wt/.git"
: >"$wt/.claude/settings.json"
: >"$wt/.envrc"

pins=(
  "$common/hooks" "$common/config" "$common/info"
  "$admin/config.worktree" "$admin/commondir" "$admin/gitdir"
  "$wt/.git" "$wt/.claude" "$wt/.envrc"
)
args=(--unshare-user --unshare-pid --ro-bind / / --dev /dev --proc /proc
  --bind "$wt" "$wt" --bind "$common" "$common")
for pin in "${pins[@]}"; do
  args+=(--ro-bind "$pin" "$pin")
done

fail=0
try_write() {
  if bwrap "${args[@]}" -- sh -c "printf x >> '$1'" 2>/dev/null; then
    echo "FAIL: sandbox could write $1" >&2
    fail=1
  fi
}
# `hooks/pre-commit` does not exist yet: creating it under the read-only hooks
# pin must fail too.
try_write "$common/hooks/pre-commit"
try_write "$wt/.git"
try_write "$admin/config.worktree"
try_write "$wt/.claude/settings.json"
try_write "$wt/.envrc"

if ! bwrap "${args[@]}" -- sh -c "printf x > '$wt/ordinary.txt'" 2>/dev/null; then
  echo "FAIL: positive control: an ordinary worktree file must stay writable" >&2
  fail=1
fi
exit "$fail"
