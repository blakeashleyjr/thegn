# Design

## Overview

Three cohesive capabilities, designed to compose: a **pipeline** that runs agent
validation stages in an ephemeral worktree, a **review-gate** that models the
findings those stages produce and how they are resolved, and **change-intent**
that gives the review/PR a deterministic statement of what the change was for.

## Why not copy the `no-mistakes` transport

`no-mistakes` intercepts `git push` via a bare gate repo + a pinned post-receive
hook, orchestrated by a background daemon. superzej is **already** the
long-running compositor and owns the UI, so none of that is needed:

- The pipeline is triggered by a panel/palette action ("Validate branch") or
  automatically on agent-task completion.
- There is no second remote, no `core.hooksPath` pinning, and no
  systemd/launchd/schtasks service — those would be redundant moving parts that
  fight superzej's one-process / one-session model.

## Render impact

- Review-gate findings surface in the **existing diff/review pane** (T 260) and
  in **`Section::Problems`** — both already part of the chrome.
- A gate (a pause awaiting a decision) is a chrome state change ⇒ it triggers a
  `Full` frame **only when the gate state changes**, never per pipeline tick.
  This respects the damage-region invariant: idle ⇒ `Skip`, pane output ⇒
  `Panes`, chrome/overlay change ⇒ `Full`.
- The pipeline runs **off the loop** on `spawn_blocking`; each stage transition
  and finding sends on a tokio mpsc channel and pulses the `TerminalWaker`, and
  the loop re-renders only when dirty. **No blocking I/O on the loop and no
  polling timeout** — the run does not introduce a tick.

## Ephemeral worktree

- Created via `GitBackend` under a reserved naming scheme (e.g. a
  `.szpipeline/<run-id>` prefix) and **not** registered as a sidebar tab, so it
  never appears in the workspace tree.
- git remains the source of truth; the worktree is on the host like any other.
- GC'd on pipeline completion (success, failure, or cancel) by reusing the
  existing stale-worktree cleanup seam (`worktree::clean_target`, roadmap item
  48). A crashed run leaves a reserved-prefix worktree that the same GC reclaims.

## State / DB

- A cache table records pipeline runs and their findings (mirrors the
  runs/steps/findings shape `no-mistakes` keeps in SQLite), so a gate survives a
  restart and the review pane can rehydrate.
- This is a **cache + resurrection** layer only — git and the agent session
  remain authoritative. Adding the table **requires a `user_version` bump**.

## Finding model

- `severity ∈ {info, warning, error}` × `action ∈ {auto-fix, ask-user}`.
- Per-step `auto_fix_limit`: how many `auto-fix` findings a stage may apply
  automatically. **Review defaults to `0`** ⇒ blocking/`ask-user` review findings
  **park** for a decision rather than self-applying.
- `info` mechanical findings (format/lint/import-sort/doc-stub) auto-apply — this
  preserves the current "edits auto-apply by design" default for the safe class
  while carving out the intent-touching class for explicit review.
- Resolution actions: **approve** (accept as-is), **fix** (apply the suggested
  change), **skip** (drop the finding). Surfaced to the human via the review pane
  and exposed to superzej's **own embedded agent** through an ACP-shaped
  structured contract (aligns with R 232) — not a separate standalone CLI.

## Change-intent

- Derived from the **agent-session↔worktree binding** superzej already maintains
  (the embedded agent runs bound to a worktree; sessions persist/resurrect). The
  intent is the agent's task/prompt for the session that produced the change —
  available directly, with **no transcript-scraping or file-overlap heuristics**.
- When no bound agent session exists (e.g. a hand-edited worktree), intent is
  simply absent; the review/PR proceeds without it (degrade, don't fail).
- Consumed by the review-gate (findings are judged against intent) and by the PR
  stage (generates the intent/changes/risk/evidence sections of the PR body),
  reusing the existing `superzej pr create` path.

## AI-additive invariant

The pipeline MUST run with AI strictly additive:

- With no agent/proxy configured, the pipeline runs only the deterministic,
  non-AI stages — the configured `test` / `lint` / `format` commands plus
  pre-commit hooks — and skips the AI stages (intent / AI review / change
  explanation).
- The shell never hard-depends on the AI layer; AI stages enrich the pipeline but
  are never required for it to run or to open a PR.
