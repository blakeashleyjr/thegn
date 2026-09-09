# Tasks — nest sidebar pipelines

- [x] Build unambiguous project attribution from direct worktree targets with a
      sibling-directory fallback.
- [x] Drop unattributable lanes from the sidebar while retaining them on the
      complete pipeline board.
- [x] Emit `Pipelines` only beneath a project and emit no pipeline rows in flat
      layout.
- [x] Remove the root unfiled group and redundant pipeline-summary row, fields,
      rendering, handlers, and summary fold.
- [x] Preserve the independent `open-pipeline-board` action and `Alt b` door.
- [x] Cover direct, sibling, ambiguous/unattributed, flat, and no-depth-zero
      cases with tests.
- [x] Update sidebar help and land the accepted implementation (`36947a32`).
- [x] Strictly validate this change.

## Validation boundary

The merge and focused tests are evidenced; this archive record does not claim a
separate historical full-CI invocation beyond those recorded gates.
