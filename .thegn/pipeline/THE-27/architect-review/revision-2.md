# THE-27 architect revision 2

The implementation still has semantic gaps in the full-screen diff lifecycle,
Files selection model, and cache identity contract. Correct these gaps and add
focused regression tests before requesting approval.

## 1. DiffView cannot acquire a review snapshot after opening (high)

- `crates/thegn-host/src/actions.rs:1004-1039` seeds `DiffView` only from the
  current `FrameModel.panel.review_snapshot` and starts only the worktree diff
  fetch.
- `crates/thegn-host/src/actions.rs:988-994` delivers `review: None` from that
  fetch, and there is no subsequent `DiffView` delivery path for a snapshot
  written by the background PR refresh or loaded by a later model hydration.
- Therefore opening the Changes/diff modal before the review cache is populated
  leaves `review` absent for the modal's lifetime; `Tab` cannot select PR review
  even after the panel later has a compatible snapshot. This misses the primary
  issue outcome on a cold start.

Add a generation-safe off-loop review cache/live delivery for an already-open
`DiffView`, or route the model-refresh delivery into the active view. Preserve
Worktree as the default, validate branch/PR/head identity, pulse the waker, and
add a test that opens without a snapshot, applies a later compatible snapshot,
and then successfully switches to PR review.

## 2. Outdated/general Files rows are painted but not selectable (high)

- `crates/thegn-host/src/review_rows.rs:18-39` emits only hunk, diff, and
  exact-anchor thread rows.
- `crates/thegn-host/src/pr_view.rs:296-299` derives the expanded Files row
  count from `open_file_rows`, while `crates/thegn-host/src/pr_view.rs:1127-1143`
  iterates that same list for selection and actions.
- `crates/thegn-host/src/pr_view.rs:1148-1174` paints outdated and general
  feedback after the file rows, but those rows never enter the selectable model.
  Consequently cursor navigation cannot reach them, `p` rejects them, and
  `r`/thread reply and the Files-side thread navigation cannot target them.

Represent the explicit outdated/general block in the shared selectable row
model (or provide an equivalent indexed selection model), and make rendering,
`row_count`, `p`, reply, and `n/N` use that same model. Keep no-anchor behavior
honest for Enter, and add regression coverage proving an outdated Files row can
be selected and handed off/replied to while still reporting no diff anchor.

## 3. Review-cache branch identity is inconsistent (high)

- `crates/thegn-host/src/hydrate.rs:3581` writes the background refresh snapshot
  with `pr.head_ref_name`.
- `crates/thegn-host/src/hydrate.rs:2930-2933` accepts a cached snapshot only
  when its branch equals `panel.branch`, the local worktree branch.
- `crates/thegn-host/src/actions.rs:733-736` applies the same local-branch
  expectation when the PR modal reads the cache, while
  `crates/thegn-host/src/actions.rs:829-837` passes that local branch to the
  view fetch.

For a checkout whose local branch name differs from the PR's remote head ref
(notably fork/remote workflows), the background writer produces a complete
snapshot that every reader rejects as stale. Define one canonical identity
(prefer the local worktree branch already used by `pr_cache`, or consistently
use the remote head ref) and use it in every writer, reader, and handoff. Add a
test with different local and remote head names proving a complete refresh is
accepted and presented only for the matching PR/head.

## Verification expected

Re-run the focused PR/diff/handoff tests and the architect land gates after the
fix. Keep the existing confirmation gate, off-loop fetch/cache writes, exact
anchor policy, and no-auto-submit behavior intact.
