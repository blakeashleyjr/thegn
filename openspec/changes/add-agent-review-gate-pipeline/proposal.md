# Add agent review-gate pipeline

## Summary

Adopt three ideas — validated against the external tool
[`no-mistakes`](https://github.com/kunchenguid/no-mistakes) — that give shape to
superzej's planned-but-unstarted agent review/merge work:

1. **Agent validation pipeline in an ephemeral worktree** — an ordered set of
   stages (review → test → lint → docs → PR) that runs in a throwaway,
   non-tab worktree so the user's working directory and open tabs are never
   disturbed.
2. **Review-gate finding model** — findings carry a `severity` × `action` pair
   with a per-step auto-fix limit, so mechanical fixes apply automatically while
   anything that touches intent _parks_ for an explicit approve / fix / skip
   decision.
3. **Change-intent attached to review/PR** — "what this change was trying to do,"
   derived deterministically from superzej's native agent-session↔worktree
   binding (not by scraping transcripts), surfaced in review and used to generate
   the PR body.

The transport layer that `no-mistakes` uses (a bare gate repo, a pinned
post-receive hook, and a background daemon) is deliberately **not** adopted:
superzej is already the long-running compositor and owns the UI, so it triggers
the pipeline from a panel/palette action or on agent-task completion.

## Impact

Roadmap items (tasks.md) this change gives concrete behavior to:

- **Q 211** — Create task (prompt/spec)
- **Q 212** — Task→worktree→agent→review→merge pipeline
- **T 262** — Inline comments → follow-up prompt
- **T 263** — Approve→merge / reject→discard
- **T 266** — AI change explanation (sem + LLM)
- **T 269** — PR creation from review
- **AR 581** — Eval hooks gate any risky transform

Relates to (should be ACP-shaped rather than a parallel mechanism):

- **R 232** — ACP permission requests → UI
- **R 233** — ACP diff rendering into the review pane (T 260)

New capabilities introduced (ADDED specs): `agent-pipeline`, `review-gate`,
`change-intent`.

## Rationale

- **superzej is _inside_ the agent.** `no-mistakes` works hard to _recover_
  intent from outside (transcript readers + file-overlap scoring + disambiguation).
  superzej already binds agent sessions to worktrees, so intent attaches
  deterministically and cheaply — the highest value-to-effort transfer.
- **The ephemeral worktree keeps the user undisturbed.** superzej is
  worktree-native and already spins per-worktree sandboxes; running the pipeline
  in a reserved, non-tab worktree is a natural fit and reuses the existing
  stale-worktree GC seam.
- **The finding model is the missing middle ground.** Today the agent edit path
  is binary (auto-apply by design); the review-gate is manual-only. `severity ×
action` with an auto-fix limit keeps mechanical fixes automatic (the current
  default for `info`) while parking intent-touching changes for a decision.

## Non-goals

- No bare gate repo, pinned `core.hooksPath`, or background daemon — redundant
  with the single-process compositor model.
- No heuristic transcript-scraping for intent — only relevant when the tool lives
  outside the agent.
- No AI hard-dependency: the pipeline MUST degrade to the non-AI checks
  (test/lint/format + pre-commit hooks) when no agent/proxy is configured.
