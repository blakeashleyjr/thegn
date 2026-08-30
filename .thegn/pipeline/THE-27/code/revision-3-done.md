# THE-27 revision 3 completion

Implemented every finding in `architect-review/verdict-3.md`.

## Fixed

- Reworked the shared host review-row projection in `review_rows.rs`. Expanded
  files now include only their inline threads, their own outdated rows, and
  the general rows; file-list feedback remains one block. PR Files and DiffView
  row counts, rendering, Enter, handoff, and reply lookup use the same model.
- Kept Worktree DiffView rows file-only, so review feedback cannot create
  invisible Worktree selections.
- Added submitted reviews to the PR-review DiffView top-level feedback block,
  alongside top-level comments, with review-state styling and wrapping.
- Moved review row construction/rendering, wrapping, lookup, diff-line helpers,
  and review-state markers into `review_rows.rs`; `pr_view.rs` remains the modal
  controller/orchestrator.
- Preserved the already-landed structural-mode source-toggle guard: `Tab` is
  inert while structural output is active, so the rendered source and footer
  cannot diverge.

## Tests and checks

- Passed targeted PR Files regression:
  `pr_view::tests::expanded_feedback_rows_share_scope_with_selection_and_rendering`
- Passed targeted DiffView regression:
  `diff_view::tests::pr_review_diff_renders_comments_and_submitted_reviews`
- Passed `just quick thegn-host` with `RUSTC_WRAPPER=` and
  `XDG_RUNTIME_DIR=/tmp` because the default sccache/runtime paths are not
  writable in this environment.
- Pre-commit treefmt, shellcheck, and yamllint hooks passed on the commits.

## Unverified

- The initial quick/test attempts using the default environment were blocked by
  sccache `Operation not permitted` and read-only `/run/user/1000`; the
  writable-path retry passed.
- Full workspace tests, e2e, `just ci`, migrations, live forge/pane/headless
  integrations, and live-state binary probes were not run per the lane policy.
- `treefmt` was exercised by the commit hook; no separate full-workspace gate
  was run.

## Disputed

None.
