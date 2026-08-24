#!/usr/bin/env bash
# PreToolUse guard: keep an AI agent off the full-workspace gates while iterating.
#
# CLAUDE.md's dev-loop policy says to iterate with `just quick <crate>` and to
# run the heavy gates once, when preparing to push. That is prose, and prose
# loses to habit: a full-workspace compile is the most expensive thing this
# machine can do, and several worktrees doing it at once is what pins all 24
# cores and drives the box into swap. This makes the policy mechanical.
#
# Reads the Claude Code hook payload on stdin and inspects `.tool_input.command`.
# Exit 2 blocks the call and feeds stderr back to the model, which is what makes
# the suggestion land rather than just the refusal.
#
# The escape hatch is deliberate and cheap: prefix the command with
# THEGN_ALLOW_HEAVY=1. Pre-push/pre-PR runs are legitimate — the point is that
# they should be a decision, not a reflex. The git hooks are unaffected either
# way; they run outside this harness.
set -uo pipefail

payload=$(cat)
command -v jq >/dev/null 2>&1 || exit 0 # no jq: never block on a broken guard
cmd=$(printf '%s' "$payload" | jq -r '.tool_input.command // empty' 2>/dev/null) || exit 0
[ -n "$cmd" ] || exit 0

# Explicit opt-in anywhere in the command line.
case "$cmd" in
*THEGN_ALLOW_HEAVY=1*) exit 0 ;;
esac

# Blank out quoted spans before matching. A guard that fires on the CONTENTS of
# a string is worse than no guard — `grep "just test\|..." CLAUDE.md` and
# `git commit -m "run just ci before pushing"` are not test runs, and the `|`
# inside such a pattern otherwise reads as a pipeline separator (this is not
# hypothetical: it blocked a plain grep the first time the guard ran). Real
# invocations are unquoted, so this costs nothing and removes the whole class.
# Not a shell parser — deliberately: over-permissive is the right failure
# direction for a guard whose job is to nudge, with the git hooks as the
# actual correctness gate.
#
# Heredoc bodies go the same way, and for a sharper reason: `git commit -F -`
# with a heredoc is how a commit message gets written, and a commit message
# about the dev-loop policy necessarily NAMES the gates it is describing. This
# guard blocked its own commit before the rule existed. Everything from the
# first `<<` introducer is dropped — a heredoc is nearly always last, and a
# missed gate after one is a far cheaper error than being unable to describe
# your own change.
scan=$(printf '%s' "$cmd" |
  sed -e '/<<-\{0,1\}[[:space:]]*['"'"'"]\{0,1\}[A-Za-z_]/,$d' \
    -e "s/'[^']*'/''/g" -e 's/"[^"]*"/""/g')

# The full-workspace gates. `just quick` is deliberately absent — that is the
# recipe this guard exists to steer people toward.
# `--command`/`exec`/`time` count as command boundaries too: around here these
# gates are almost always reached as `nix develop --command just test`, and a
# guard that only sees the leading token would miss every real invocation.
heavy_re='(^|[;&|(]|&&|--command|[[:space:]](exec|time|nice))[[:space:]]*(just[[:space:]]+(test|test-doc|ci|ci-local|coverage|coverage-html|lint|bench|bench-micro|e2e|doc-check)([[:space:]]|$)|cargo[[:space:]]+llvm-cov)'
workspace_re='cargo[[:space:]]+(build|check|clippy|test|nextest[[:space:]]+run)([[:space:]]|$).*--workspace'

# The one case blanking quotes would otherwise hide: a gate handed to a shell as
# a quoted script (`nix develop --command bash -lc "just build && just test"`).
# Matched on the RAW text, but only behind an actual shell NAME plus a -c-ish
# flag — that is what keeps it from colliding with `grep -c "just test"`, which
# has a flag but no shell.
runner_re='(bash|sh|zsh|dash)[[:space:]]+-[a-z]*c[[:space:]]*.*(just[[:space:]]+(test|test-doc|ci|ci-local|coverage|lint|bench|e2e|doc-check)([[:space:]]|"|$)|cargo[[:space:]]+llvm-cov)'

if printf '%s' "$scan" | grep -qE "$heavy_re" ||
  printf '%s' "$scan" | grep -qE "$workspace_re" ||
  printf '%s' "$cmd" | grep -qE "$runner_re"; then
  cat >&2 <<'EOF'
Blocked: that is a full-workspace gate, and this box runs several thegn
worktrees at once — each full compile is what pins the CPU and pushes the
machine into swap.

While iterating, use the scoped equivalents instead:

  just quick <crate>                        typecheck + clippy, lib/bin only
  cargo nextest run -p <crate> <substring>  just the tests you are touching
  cargo check -p <crate>

Run the heavy gates ONCE, when you are preparing to push or open a PR — and
let the pre-push hook (clippy + test + smoke) be the thing that runs them.

If this really is that moment, say so explicitly:

  THEGN_ALLOW_HEAVY=1 <your command>
EOF
  exit 2
fi
exit 0
