# THE-32 architect review revision 1

The implementation needs a new coding pass for two correctness and safety gaps.

## 1. Enrich merge-tree gitlink conflicts from the merge inputs

Relevant files: `crates/thegn-svc/src/git/plumbing.rs`,
`crates/thegn-svc/src/git/submodule.rs`, and
`crates/thegn-host/src/integrate.rs`.

`merge-tree --write-tree --name-only -z` operates against the object database and
does not populate the current worktree index. The current
`submodule_conflicts_for_paths` forwarding path calls `git ls-files -u -z`, so a
clean index returns no staged conflict records even when the synthetic
`merge-tree` result contains a mode-160000 conflict. The host then either loses
the typed conflict or proceeds to regeneration/custom-driver/rerere handling.

Change the plumbing seam so conflict enrichment has the `ours` and `theirs`
merge inputs (and the merge base where needed), and resolve each conflicted path
from those trees/refs using `git ls-tree` or equivalent object-database reads.
Only mode-160000 entries should become `SubmoduleConflict` values, carrying the
actual ours/theirs object IDs and the existing typed detail fields. Preserve
path-safe NUL parsing and handle missing/erroring metadata distinctly from “not
a gitlink”. If metadata cannot be read, fail closed by deferring the conflict;
do not treat the error as an empty submodule-conflict list.

Partition gitlink paths before every regenerate, custom-driver, rerere, or
blanket-stage path. A gitlink conflict must never be sent through those
auto-resolution paths. Add an integration fixture with divergent gitlink tips
in the synthetic merge inputs and assert that the result is typed and deferred,
with no driver/rerere attempt and no broad staging.

## 2. Make forge and local diff gitlinks atomic and non-selectable

Relevant files: `crates/thegn-host/src/pr_view.rs`,
`crates/thegn-host/src/diff_view.rs`, and the shared forge/panel model only if
needed.

The forge parser now records `DiffFile::is_submodule`, but neither the PR Files
view nor the local diff view consumes that marker. Both views still flatten and
render all hunk lines, so a gitlink’s `Subproject commit ...` text is presented
as an ordinary line diff and remains eligible for selection and inline
commenting.

Render a submodule change as one atomic pointer row (including the pointer
metadata/status required by the design), without exposing synthetic hunk lines
as selectable rows. Navigation, row counts, selection highlighting, and inline
comment creation must all treat that row as non-commentable; ordinary files
must retain their current behavior. Add focused host tests for both PR and
local diff views covering rendering, navigation/row counts, and rejection of an
inline comment target on a submodule pointer.

## Acceptance checks

- `cargo fmt --all -- --check`
- focused core/service/host tests for submodule parsing, merge conflict
  enrichment, PR files, and local diff behavior
- `just quick thegn-host`
- an end-to-end fixture demonstrates that divergent mode-160000 pointers are
  deferred safely and that no submodule pointer is treated as a line-comment
  target.
