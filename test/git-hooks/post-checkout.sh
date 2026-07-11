#!/bin/sh
# thegn: make the devenv-generated prek config available in every worktree.
#
# The shared git hooks (core.hooksPath -> the canonical checkout's .git/hooks)
# run in every worktree, but prek needs .pre-commit-config.yaml in the worktree
# root. That file is a gitignored Nix-store symlink devenv only materializes in
# the checkout where `devenv shell` was entered, so it's missing in every other
# worktree and prek aborts with "config file not found" -- which is why commits
# and merges in worktrees used to need --no-verify.
#
# git fires post-checkout after `git worktree add` (thegn creates worktrees
# via the git CLI) and on branch checkout, with cwd = the worktree, so we seed a
# chained symlink to the canonical checkout's config here. Chained (rather than
# resolved to the store path) so it auto-follows devenv re-locks. Idempotent and
# a safe no-op when the source isn't available. post-checkout is not a
# prek-managed stage, so prek/devenv hook re-installs won't clobber it.
#
# Installed into the effective hooks dir by devenv.nix's enterShell.

# First, defensively strip any stray `core.worktree` that an external worktree
# tool (herdr) or a GIT_*-exporting child leaked into the shared `.git/config`
# since the last heal -- post-checkout fires right after `git worktree add` /
# branch checkout, the moments such a leak most often lands, and while the
# target path usually still exists (so git isn't yet wedged). Pure-text, no git;
# a safe no-op when clean. Best-effort: never block the checkout. (cwd is the
# worktree top, where the tracked script lives; thegn heals the same key
# in-process, and `just heal-git` covers the wedged/dangling case.)
if [ -f test/git-hooks/heal-worktree.sh ]; then
  sh test/git-hooks/heal-worktree.sh >/dev/null 2>&1 || true
fi

cfg=.pre-commit-config.yaml

# Already have a live config (real file or working symlink) -- nothing to do.
# This also short-circuits the canonical checkout, whose config is a live symlink.
if [ -L "$cfg" ] && [ -e "$cfg" ]; then
  exit 0
fi

common=$(git rev-parse --git-common-dir 2>/dev/null) || exit 0
canonical=$(cd "$(dirname "$common")" 2>/dev/null && pwd) || exit 0
src="$canonical/$cfg"

# Only link when the canonical config resolves to a real file, and never to self.
if [ "$src" != "$(pwd)/$cfg" ] && [ -e "$src" ]; then
  ln -sf "$src" "$cfg"
fi
exit 0
