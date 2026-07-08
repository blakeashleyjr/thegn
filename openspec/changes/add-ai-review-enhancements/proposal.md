# Add AI review enhancements

## Summary

A cluster of **AI-additive niceties** on the review/commit surface, each of which
degrades cleanly to a plain non-AI fallback so the AI-free shell never grows a
hard dependency:

1. **Annotate-AI-diff batch loop** — inline diff comments pinned across edits,
   collected and dispatched to the embedded agent as a single follow-up prompt
   ("send all to agent").
2. **AI commit-message draft** — generate a commit message from the staged diff
   via the proxy; with AI off, fall back to a deterministic template / empty
   editor (hooks always run; `--no-verify` is never forced).
3. **"Fix with AI" on failed checks** — when a pre-commit hook or a CI check
   fails, hand the failing output (hook text, or `CiRun` failed jobs +
   `CiLog::first_failure_line`) to the agent as a repair prompt; with AI off, the
   action simply shows the failing output.
4. **Image-diff modes** — swipe / onion-skin comparison of changed images in the
   diff pane via the existing graphics preview path. This one is **AI-free** and
   has no AI fallback to define.

## Impact

Roadmap items (tasks.md) this change gives concrete behavior to:

- **T 763** — Annotate-AI-diff batch loop (inline comments pinned across edits,
  "send all to agent" as one follow-up prompt; extends 262; ties ACP
  `session/prompt`)
- **T 764** — AI commit-message draft (generate from the staged diff via the
  proxy; template fallback when AI is off)
- **T 765** — "Fix with AI" on failed pre-commit hooks / failed CI checks (hand
  hook output / `CiRun` failed jobs + `CiLog::first_failure_line` to the agent;
  never `--no-verify`)
- **T 766** — Image-diff modes (swipe / onion-skin via the graphics preview path)

Relates to:

- **T 262** — Inline comments → follow-up prompt (763 extends it)
- **AR 654** — agent review/repair surface
- **U** (group) — the `superzej-proxy` LLM proxy used for every model call
- **AV** (group) — CI: `crates/superzej-svc/src/ci.rs` `CiRun`/`CiLog`
- **AF 399** — graphics preview (kitty/iTerm/sixel) reused for image-diff
- **Y** (group) — commit/push flow in `gitmut.rs`

New capability introduced (ADDED spec): `agent-review`.

## Rationale

- **The seams already exist.** The diff/review pane (T 260), visual hunk staging
  in `gitmut.rs`, the proxy, and the `CiRun`/`CiLog` model are all shipped; these
  features are thin AI overlays on top of them, not new subsystems.
- **AI is strictly additive.** Each model-backed action has a defined non-AI
  path, so a user with no proxy/agent configured gets a fully functional review
  and commit surface — just without the AI draft/repair conveniences.
- **Hooks stay authoritative.** "Fix with AI" repairs failing checks by re-running
  them after the agent edits; it never bypasses hooks via `--no-verify`.
- **Image-diff is pure UI.** It reuses the graphics preview path and carries no
  AI cost, so it ships unconditionally.

## Non-goals

- No new model transport — every model call goes through `superzej-proxy`
  (group U); no direct provider SDK calls.
- No `--no-verify` bypass and no auto-commit — the user still confirms the commit;
  the agent edits the tree and checks re-run.
- No AI hard-dependency: with no agent/proxy configured, commit-message draft
  falls back to a template/empty editor and "Fix with AI" just shows the failing
  output.
- No new image-format decoder — image-diff renders only formats the existing
  graphics preview path already handles.
