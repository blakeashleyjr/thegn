# Design

## Overview

Four AI-additive overlays on the existing review/commit surface, each reusing a
shipped seam and each (except the AI-free image-diff) carrying a defined non-AI
fallback:

- **Annotate-AI-diff batch loop** (T 763) — a pinned-comment store layered on the
  diff/review pane, flushed to the embedded agent as one ACP `session/prompt`.
- **AI commit-message draft** (T 764) — a proxy call over the staged diff,
  feeding the commit editor; template/empty fallback when AI is off.
- **"Fix with AI" on failed checks** (T 765) — a repair prompt assembled from
  hook output or CI failure details, handed to the agent; checks re-run, hooks
  never bypassed.
- **Image-diff modes** (T 766) — swipe / onion-skin rendering through the
  graphics preview path. AI-free.

## Reused seams

- **Diff/review pane** (T 260): the `Section`/panel render that already shows the
  hunk view; annotations and image-diff render here. Visual hunk staging lives in
  `gitmut.rs`.
- **Commit flow** in `gitmut.rs`: hooks run normally and the existing
  `--no-verify` toggle is left untouched — this change MUST NOT bypass it.
- **Proxy** `thegn-proxy` (group U): the single transport for every model
  call (commit-message draft, batch annotation prompt, fix-with-AI repair).
- **CI** `crates/thegn-svc/src/ci.rs`: `CiRun { jobs }` and
  `CiLog { text, truncated }`, plus `CiLog::first_failure_line`, supply the
  failed-job names and first-failure line for the fix-with-AI repair prompt.
- **Graphics preview path** (AF 399): the kitty/iTerm/sixel renderer used for
  file previews, reused to draw the before/after image-diff frames.

## Rendering & event loop

- Every model call and every CI/log fetch runs **off the loop** on
  `spawn_blocking`; results return over a tokio mpsc channel that **pulses the
  `TerminalWaker`**. No blocking I/O (proxy, git, subprocess) ever runs on the
  loop, and **no polling timeout / new tick** is introduced — an in-flight draft
  or repair leaves the loop idle (`Skip`) until its result lands.
- Damage mapping respects `render_plan::plan`:
  - **Skip** — idle wake while a draft/repair/CI fetch is in flight; no dirty
    state, no frame.
  - **Panes** — none of these features write to PTY panes directly; pane output
    from an agent the prompt triggers is a normal `Panes` frame (bounded-diff the
    changed pane only, no chrome recompose).
  - **Full** — adding/removing a pinned annotation, the commit editor opening
    with a draft, a "Fix with AI" status change, or switching image-diff mode are
    chrome/overlay state changes ⇒ a `Full` frame **only on the transition**,
    never per streaming tick. Image-diff redraws are confined to the diff pane's
    region.

## Annotate-AI-diff batch loop (T 763)

- Annotations are pinned to a `(path, hunk-anchor)` so they survive edits to the
  surrounding diff (re-anchored on diff refresh; dropped only when the anchored
  hunk disappears). Extends the inline-comment surface of T 262.
- "Send all to agent" collects the pinned comments into a single follow-up
  ACP `session/prompt` to the embedded agent — one prompt, not N.
- AI-off fallback: the pinned comments remain a usable plain review-note list in
  the pane; "send all to agent" is disabled/absent and no proxy call is made.

## AI commit-message draft (T 764)

- On commit, the staged diff is sent through the proxy to draft a message, which
  pre-fills the commit editor (the user always edits/confirms — no auto-commit).
- Hooks run normally on commit; the `--no-verify` toggle in `gitmut.rs` is not
  touched by this feature.
- AI-off fallback: the editor opens with a deterministic template (or empty),
  exactly as it does today; no proxy call is made.

## "Fix with AI" on failed checks (T 765)

- A failed **pre-commit hook** surfaces its captured hook output; a failed **CI
  check** surfaces `CiRun` failed-job names + `CiLog::first_failure_line`.
- "Fix with AI" assembles that failing output into an ACP repair prompt for the
  embedded agent. After the agent edits the worktree, the failing check/hook is
  **re-run**; the action **never** passes `--no-verify` and never bypasses hooks.
- AI-off fallback: the action degrades to simply **showing the failing output**
  (hook text / first-failure line) in the pane — no repair prompt, no proxy call.

## Image-diff modes (T 766)

- For a changed image in the diff, render the old (`HEAD`) and new (working)
  blobs through the graphics preview path in one of two modes: **swipe** (a
  draggable split between before/after) or **onion-skin** (alpha-blended
  overlay), toggled in the diff pane.
- AI-free: no model call, no fallback to define. Only formats the graphics
  preview path already decodes are supported; others fall back to the normal
  text/binary diff treatment.

## Persistence

- **Pinned annotations** are session-scoped review state and may be cached in a
  new table keyed by `(workspace, worktree, path, hunk-anchor)` so a partially
  annotated review survives a restart; git remains authoritative for the diff.
  Adding this table **requires a SQLite `user_version` bump**.
- Commit-message drafts, fix-with-AI prompts/results, and image-diff mode are
  transient UI state and are **not** persisted.

## Invariants

- **AI is strictly additive.** Every AI-dependent feature here has a defined
  non-AI path: commit-message draft → template/empty editor; annotate batch loop
  → plain review-note list with the agent send disabled; fix-with-AI → show the
  failing output. The shell MUST NOT hard-depend on the agent/proxy for review or
  commit to function. Image-diff is AI-free.
- **Hooks are never bypassed.** No feature passes `--no-verify`; checks re-run
  after agent edits.
- **Off-loop, waker-pulsed, no tick.** No blocking I/O on the loop; no polling
  timeout; render-plan invariants (Skip/Panes/Full) stay green.
- **git is the source of truth.** The annotation cache is a resurrection layer
  only.
