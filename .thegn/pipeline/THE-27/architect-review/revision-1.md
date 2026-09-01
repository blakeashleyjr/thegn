# THE-27 architect revision 1

The implementation is not ready to approve. Address the following semantic
findings and add focused regression tests for each behavior.

## 1. Handoff must use the actual selected thread (high)

- `crates/thegn-host/src/pr_view.rs:410-434` claims `p`/`P` globally, and
  `crates/thegn-host/src/pr_view.rs:502-510` resolves `p` through the separate
  `thread_sel` cursor.
- Normal `j`/`k` selection and selecting a thread row do not update
  `thread_sel`, so pressing `p` on the second thread can hand the first thread
  to the agent. The same key also hands off the first thread while the cursor
  is on a non-thread row.
- Fix by deriving the selected thread from the current Conversation/Files row
  (or synchronizing one selection model on every cursor transition), rejecting
  non-thread rows, and testing cursor selection followed by `p`.

## 2. Headless handoff must be confirmation-gated (high)

- `crates/thegn-host/src/review_handoff.rs:152-187` starts `agent_run` as soon
  as the key is pressed when no live pane exists.
- The OpenSpec requirement and lane contract require a confirmation before an
  unattended headless agent receives remote review text. Implement the normal
  confirmation overlay/continuation, preserving the off-loop dispatch and
  waker pulse after confirmation; `P`/the all-unresolved path must obey the
  same safety gate.
- Sync `openspec/changes/add-pr-comments-in-diff/` with the final approved
  `p`/`P` key contract and cache/fetch behavior; do not weaken the existing
  confirmation requirement merely to match the current code.

## 3. Inline diff rows hide all but the first comment (high)

- `crates/thegn-host/src/pr_view.rs:1138-1153` and
  `crates/thegn-host/src/diff_view.rs:496-517` render only
  `thread.comments.first()`.
- A `ReviewThread` carries every comment, and the design/OpenSpec require the
  full thread/comment bodies in the diff projection. Render all comments (with
  the existing wrapping/indent policy) under one selectable thread header, or
  use selectable comment rows while retaining the thread identity for reply
  and handoff. Add a multi-comment rendering test for both projections.

## 4. Source toggle can lie while structural mode is active (medium)

- `crates/thegn-host/src/diff_view.rs:208-222` handles `Tab` before the
  structural-mode branch. With a loaded structural worktree render, `Tab`
  switches `source` to `PrReview` but `structural_active()` remains true, so
  the body is still the worktree structural diff while the footer says
  `PR review`.
- Make source switching unavailable in structural mode or explicitly leave
  structural mode before selecting PR review; add a regression test asserting
  the rendered mode/source pair cannot diverge.

## Verification still unavailable

- `.understand-anything/knowledge-graph.json` is absent, so no graph overlay
  was generated.
- `openspec validate --all --strict` could not run because `openspec` is not on
  PATH. Direct `treefmt` could not initialize because `taplo` is unavailable,
  although the merge and review commits' pre-commit treefmt hooks passed.
- The lane's live forge, pane, and headless-agent integration remains
  unexercised; the required local gates passed where the environment allowed
  them. No e2e, `just test`, or `just ci` was run.
