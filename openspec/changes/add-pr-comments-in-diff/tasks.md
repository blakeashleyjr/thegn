# Tasks — PR comments in the diff + thread→agent handoff

## 1. Pure anchor + prompt logic (thegn-core)

- [ ] 1.1 New core module (`pr_threads.rs`): `anchor_threads(&PrDiff,
&[ReviewThread])` mapping threads to rendered new-side diff rows with an
      `outdated` bucket (misses and deletion-side anchors) — **unit tests**:
      hit, miss ⇒ outdated, deleted-line anchor, multiple threads on one
      line, resolved filtering (95% core gate).
- [ ] 1.2 `thread_prompt(&ReviewThread, …)` — single-thread handoff text with
      the PR rules block, bodies capped like `pr_driver::format_threads` —
      **unit tests**: formatting, caps, rules block present.
- [ ] 1.3 PTY sanitizer for paste text: strip C0/C1 except `\n`/`\t`,
      neutralize the bracketed-paste terminator — **unit tests** with hostile
      inputs (ESC/CSI/OSC, `ESC[201~` mid-body).

## 2. Files-tab interleave (thegn-host, pr_view.rs)

- [ ] 2.1 Interleave anchored thread blocks under their diff lines in the
      expanded-file view, selectable thread headers joining the flat row
      index; outdated block after the last hunk; reply composer opens from a
      thread row (reuse `ComposerTarget::ThreadReply`).
- [ ] 2.2 Resolved-threads toggle (view-local, off by default).
- [ ] 2.3 Unresolved-count markers on Files-tab file rows and the panel `Pr`
      section thread summary — glyphs via `caps::active_glyphs()`, no
      literals at draw sites (ratchet).
- [ ] 2.4 Verify composition with the stacked/per-commit walker
      (`add-pr-review-viewed-stacked`, if landed): anchoring keys off the
      rendered diff range.

## 3. Handoff key (thegn-host)

- [ ] 3.1 Handoff key on thread rows (Files + Conversation): resolve live
      agent pane → sanitized bracketed paste via `pane_writer`, no trailing
      newline, focus the pane; only the worktree's own remembered agent pane
      is ever targeted.
- [ ] 3.2 Headless fallback: confirm-gated single-thread `PrReview` dispatch
      via `agent_task` + `agent_run` off-loop (waker pulse on completion);
      `p`/`P` use the selected/all-unresolved contract; no-agent path reports
      via `model.status`.

## 4. Help + docs

- [ ] 4.1 Update `docs/help/review-a-pr.md` (context `panel:pr`) with the new
      chords (handoff, resolved toggle) — the help prose ratchet requires the
      page to mention them; update the PR view hint line.

## 5. Validation

- [ ] 5.1 e2e: re-record any PR-view snapshots a frame change touches
      (`just e2e-update`, review the diff); pin new volatile chrome in
      `e2e_freeze` if any.
- [ ] 5.2 Run `just ci` once, pre-PR (includes `openspec validate --all
--strict`).
