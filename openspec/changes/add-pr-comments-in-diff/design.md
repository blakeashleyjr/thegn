# Design — PR comments in the diff + thread→agent handoff

## Anchor matching is pure core logic

A new `thegn-core` module (`pr_threads.rs` or a sibling of `forge::model`)
owns the two pure functions the view calls:

- `anchor_threads(&PrDiff, &[ReviewThread]) -> AnchoredThreads` — for each
  file, a map of diff-row → thread indices plus an `outdated` bucket for
  threads whose `(path, line)` has no `new_lineno` match in the rendered
  hunks. Deletion-side anchors (GitHub threads on removed lines carry no
  new-side line) also land in `outdated` rather than being guessed at.
- `thread_prompt(&ReviewThread, pr: &PrSummaryFacts) -> String` — the
  handoff text: PR number/title/branch, `path:line`, the anchoring
  `diff_hunk`, each comment as `author: body`, and the PR-family rules block
  (push, `--force-with-lease`, never merge/approve/resolve). Reused verbatim
  by both the paste path and the headless dispatch (as the single entry in
  the existing `PrReview` `{threads}` variable), and by THE-22's
  `add-watched-pr-comment-tasks` later.

Both are exhaustively unit-tested (95% core gate): anchor hit, anchor miss ⇒
outdated, deletion-side ⇒ outdated, multiple threads on one line, resolved
filtering, prompt formatting bounds (bodies capped, like
`pr_driver::format_threads`).

## Files-tab interleave (pr_view.rs)

`files_body`'s expanded-file arm currently walks `hunks × lines` with a flat
selectable index. It gains a second row class: after emitting a diff line
whose `new_lineno` matches an anchored thread, the thread block renders —
header (author, `path:line`, resolved marker) plus wrapped comment bodies via
the existing `push_comment_block`/`push_body_lines` helpers. Thread headers
join the selectable-row index so the cursor can land on them; diff-line
selection (for `LineComment` composing) is unchanged. The `outdated` bucket
renders after the last hunk under an "outdated feedback" rule.

- **Resolved toggle:** unresolved-only by default; a key toggles resolved
  threads in. View-local state, no config key (a `[panel]` default knob was
  considered and rejected — one keypress, remembered per view session, is
  enough).
- **File-list markers:** the file rows (collapsed view) append an
  unresolved-count chip when nonzero; same count on the panel `Pr` section's
  thread rows it already renders. Glyphs come from `caps::active_glyphs()`
  — no literal at the draw site (color/glyph ratchet).
- **Composition with `add-pr-review-viewed-stacked`:** anchoring runs against
  the _rendered_ `PrDiff`, so in stacked (per-commit) mode threads simply
  anchor into whichever commit's diff contains their line, and miss into
  `outdated` otherwise. No coupling beyond "anchor against what you render".

## The handoff key

On a thread row (Files or Conversation tab), `p` passes the selected thread and
`P` passes all unresolved threads; the view's hint line and
`docs/help/review-a-pr.md` document the contract. Each key resolves in order:

1. The worktree's remembered agent pane is running ⇒ **paste**: format via
   `thread_prompt`, sanitize (below), bracketed-paste into that pane through
   `pane_writer`, focus the pane, no trailing newline. The human submits.
2. Otherwise, if a headless agent resolves (`agent_task::resolve_agent`
   against `[pr_queue] agent` / `agent_command`, falling back to the
   worktree's remembered agent entry) ⇒ **confirm-then-dispatch**: a
   one-line confirm (the same idiom as destructive panel actions), then a
   single-thread `PrReview` run via `agent_run` off-loop, progress in
   `model.status`, completion pulsing the waker.
3. Neither ⇒ `model.status` explains ("no agent pane running and no
   `[pr_queue] agent` configured") and nothing happens. The shell never
   hard-depends on an agent.

## Event loop, rendering, help

- **Damage:** the PR view is a full-screen modal — all its repaints are the
  master `dirty` ⇒ `Full`, unchanged. The paste path writes to a PTY (pane
  damage arrives as normal pane output ⇒ `Panes`). No new wake source; the
  headless dispatch reuses `agent_run`'s off-thread run + waker pulse.
- **Fetch/cache:** the existing off-loop review refresh reads and writes the
  complete per-worktree review snapshot, preserving the last complete cache
  entry across transient forge failures. It delivers the generation-tagged
  snapshot over `pr_view_tx`; anchoring runs at render-data assembly (on the
  loop, pure, linear in diff size — the diff is already being composed).
- **Help context:** `panel:pr` → `docs/help/review-a-pr.md` gains the new
  chords (`p`/`P`, resolved-toggle); the prose ratchet requires the page to
  actually mention them. No new `ACTION_SPECS` id — PR-view keys are
  view-internal like the existing composer keys — so no help-ratchet claim
  beyond the prose.

## Security

- **Comment bodies are untrusted remote input entering a PTY.** A teammate's
  (or fork contributor's) review comment could embed terminal control
  sequences. The paste path MUST sanitize: strip every C0/C1 control byte
  except `\n`/`\t` (in particular ESC, so no CSI/OSC survives) and neutralize
  the bracketed-paste terminator sequence (`ESC[201~`) so a body cannot
  break out of the paste. Sanitization lives next to `thread_prompt` in core
  and is unit-tested with hostile inputs.
- **Prompt injection.** The same bodies become agent instructions. The human
  reviews before submit on the paste path (no trailing newline is a security
  property, not just UX); the headless path is confirm-gated per keypress
  and carries the PR rules block. The agent may never merge, approve,
  resolve threads, or force-push without lease — inherited from the
  `PrReview` prompt contract and, structurally, from the fact that the agent
  holds no forge credentials (it works in the checkout; forge writes go
  through thegn's own seam calls).
- **Pane targeting.** Paste only ever targets the worktree's own remembered
  agent pane — never the focused pane, never another worktree's — so a
  stray keypress cannot type into an arbitrary shell.
- **No new network writes.** Replies still go through the existing composer
  actions; the handoff itself is local.

## Alternatives considered

- **Render threads in the panel `Changes` (working-tree) diff too** —
  rejected for this change: anchors are on the PR head, the working tree has
  drifted, and a wrong line is worse than a tab switch. Non-goal, revisit
  with a blame-based re-anchor if demanded.
- **Auto-submit the pasted prompt** — rejected; see Security. THE-22 is the
  sanctioned autonomous path with its own safety rails.
- **A new TaskKind for single-thread dispatch** — rejected; `PrReview` with
  a one-entry `{threads}` variable is the same task, and a new kind would
  fan out across config prompts, validation, and the pinned-count tests for
  no behavioral difference.

## Open questions

- Should the outdated bucket offer a "jump to Conversation" rather than
  rendering full bodies twice? (Leaning: render in place; duplication is
  cheap and the point is fewer tab switches.)
- Key choice `g`/`G` vs `a` (taken by section actions elsewhere) — settle at
  implementation against the PR view's existing hint line.
