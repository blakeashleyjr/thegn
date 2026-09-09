# Tasks — folder-aware sidebar ordering

- [x] Add the pure sibling-run ordering model, edge crossing/refiling, collapsed
      folder handling, home anchoring, folder ordering, and focused tests.
- [x] Add atomic exact-order persistence for worktrees and folders without a DB
      schema change.
- [x] Route keyboard reordering through cursor/path identity, including dormant
      projects and folder headers, with off-loop persistence.
- [x] Route mouse worktree/folder drops through the same run model, preserve flat
      membership, and remove the partial step-swap loop.
- [x] Cover partitioning, boundaries, persistence, handlers, folder drops, and
      synthetic-id guards with unit tests.
- [x] Document the reorder and mouse semantics; land as `46b9448c` and strictly
      validate this change.

## Validation boundary

No separate historical live-TUI walkthrough or end-of-branch full-CI result is
claimed. The accepted deterministic ordering/persistence contracts are covered
by the recorded tests.
