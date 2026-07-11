#!/bin/sh
# thegn: strip a stray `core.worktree` from a MAIN checkout's shared
# `.git/config`.
#
# A non-bare main checkout must NEVER carry core.worktree: it silently
# retargets EVERY git read/write at the named directory. A child `git`
# invocation that runs with GIT_DIR/GIT_WORK_TREE exported (an agent shell, an
# external worktree tool like herdr, a worktree op inside a git hook) can leak
# it into the shared config. Worse, once the target path is DELETED, git
# canonicalizes the value on every read and aborts (`git config
# --get/--unset/--list`, and even `git commit`, die with "Invalid path" /
# "must be run in a work tree") -- so git itself can no longer repair it, and a
# pre-commit hook can't help because git dies before the hook runs.
#
# This is the pure-text repair: it never invokes git, resolves the shared
# `.git/config` from any worktree by reading `.git` / `commondir` directly, and
# drops only the `worktree` key inside the `[core]` section, preserving every
# other byte. It mirrors `thegn_core::util::strip_core_worktree` so the
# in-process (thegn) heal and this out-of-process heal agree.
#
# Usage: run from anywhere inside the checkout. `-v` prints when it heals.
#   sh test/git-hooks/heal-worktree.sh [-v] [dir]
# Exit 0 always (a stray key found+stripped, or nothing to do); it is a safe,
# idempotent no-op on a clean config and on a linked worktree's own config
# (whose `.git` is a file and legitimately sets core.worktree).
set -eu

verbose=0
case "${1:-}" in
-v | --verbose)
  verbose=1
  shift
  ;;
esac
start="${1:-.}"

# Resolve the shared (main checkout) `.git` directory from `start`, using only
# filesystem reads -- git may be wedged by the very key we are here to strip.
dotgit="$start/.git"
gitdir=""
if [ -d "$dotgit" ]; then
  # Main checkout: `.git` is a directory and IS the shared git dir.
  gitdir=$dotgit
elif [ -f "$dotgit" ]; then
  # Linked worktree: `.git` is a file `gitdir: <per-worktree-gitdir>`. Its
  # `commondir` points at the shared `.git`.
  p=$(sed -n 's/^gitdir: *//p' "$dotgit" | head -n1)
  [ -n "$p" ] || exit 0
  case "$p" in
  /*) wtgit=$p ;;
  *) wtgit="$start/$p" ;;
  esac
  if [ -f "$wtgit/commondir" ]; then
    cd=$(cat "$wtgit/commondir")
    case "$cd" in
    /*) gitdir=$cd ;;
    *) gitdir=$(
      unset CDPATH
      cd -- "$wtgit/$cd" 2>/dev/null && pwd
    ) || exit 0 ;;
    esac
  else
    # Fallback: `<wtgit>/../..` is `<canonical>/.git`.
    gitdir=$(
      unset CDPATH
      cd -- "$wtgit/../.." 2>/dev/null && pwd
    ) || exit 0
  fi
else
  exit 0
fi

cfg="$gitdir/config"
[ -f "$cfg" ] || exit 0

# Drop `worktree = ...` from the `[core]` section only; a subsection like
# `[core "x"]` or any other header leaves the section. Byte-for-byte otherwise.
# awk exits 3 when nothing was removed so we skip the rewrite entirely.
tmp="$cfg.heal.$$"
if awk '
  BEGIN { removed = 0 }
  {
    line = $0
    t = line
    sub(/^[ \t]+/, "", t)
    if (t ~ /^\[/) {
      hdr = t
      sub(/\].*$/, "", hdr)
      sub(/^\[/, "", hdr)
      gsub(/[ \t]/, "", hdr)
      in_core = (tolower(hdr) == "core")
      print line
      next
    }
    if (in_core) {
      key = t
      sub(/[ \t=].*$/, "", key)
      if (tolower(key) == "worktree") { removed = 1; next }
    }
    print line
  }
  END { if (!removed) exit 3 }
' "$cfg" >"$tmp" 2>/dev/null; then
  # A key was removed -- swap in the cleaned config atomically.
  cat "$tmp" >"$cfg"
  rm -f "$tmp"
  [ "$verbose" -eq 1 ] && echo "heal-worktree: stripped stray core.worktree from $cfg" >&2
  exit 0
fi
# awk exited non-zero (3 = nothing to remove, or a read error): leave config
# untouched and clean up.
rm -f "$tmp"
exit 0
