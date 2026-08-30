# THE-27 architect revision 3

The implementation passes the focused and mandatory available gates, but the
review/data model still diverges from what the UI paints. Correct these
semantic gaps and add focused regression tests before requesting approval.

## 1. Expanded Files rows and DiffView row counts must share one model (high)

- `crates/thegn-host/src/pr_view.rs:331-343` appends the entire
  `feedback_rows(review, ...)` collection to every expanded file. The same
  duplicated collection is painted at `pr_view.rs:1201-1228`, while the
  file-list-only block is separately painted at `pr_view.rs:1232-1247`.
- `pr_view.rs:540-543` obtains the `p` handoff row only through
  `selected_files_feedback_row`, and `pr_view.rs:710-713` deliberately returns
  `None` while a file is expanded. Thus an outdated/general row that is
  selectable and visible in an expanded file cannot be handed off with `p`.
  Reply happens to use a different path (`pr_view.rs:690-693`), so the action
  model is inconsistent as well.
- `crates/thegn-host/src/diff_view.rs:149-171` adds the whole feedback block to
  the expanded-file row count, but `diff_view.rs:529-565` paints that block only
  when no file is open. Cursor movement can therefore land on invisible rows.
  The same unconditional addition at `diff_view.rs:149-156` changes the
  default Worktree file-list row count even though the Worktree body does not
  render review feedback.

Build one shared selectable row model with the same scope as its renderer:
inline threads plus that file's outdated rows while a file is open, and one
outdated/general block at the appropriate file-list/end-of-file location. Make
`row_count`, rendering, `p`, `r`, `n/N`, and Enter all consume that model. Keep
the Worktree source exactly unchanged. Add tests for an expanded outdated row
and an expanded general row proving `p` and reply target the selected thread,
Enter reports no anchor, and no invisible rows exist in either source mode.

## 2. DiffView must render the complete top-level feedback snapshot (medium)

- `crates/thegn-host/src/diff_view.rs:530-543` renders only
  `snapshot.conversation.comments`. It omits `conversation.reviews`, despite
  the snapshot contract carrying submitted reviews and the design requiring a
  top-level feedback block in PR-review mode.

Render submitted reviews with the existing review-state styling/wrapping (or
reuse a shared top-level feedback projection), and add a test containing both a
top-level comment and a submitted review that asserts both appear in the PR
review DiffView body.

## 3. Keep the PR view orchestration out of the god file (medium)

`crates/thegn-host/src/pr_view.rs` grew from 1,305 lines on `main` to 1,889
lines in this branch. The new review row construction, selection, wrapping,
and rendering spans `pr_view.rs:331-803` and is still coupled to the modal
controller, despite the design explicitly requiring a small host projection
module and no god-file growth. Extract the review-specific row/action/render
helpers into the existing `review_rows.rs` or a narrowly-scoped sibling while
leaving `PrView` as orchestration. Do not solve this with a new broad utility
module or duplicated diff parser.
