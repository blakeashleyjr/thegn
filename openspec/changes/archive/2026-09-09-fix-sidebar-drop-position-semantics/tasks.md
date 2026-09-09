# Tasks — sidebar drop-position semantics

- [x] Extract pure reorder/drop seams and record regressions for exact hovered
      slot and tail reachability across row heights.
- [x] Replace half-row math with the row-slot/displacement model for worktrees,
      folders, and workspaces.
- [x] Freeze drag layout, re-resolve live placement, add proportional bounded
      edge scroll, pointer capture, Escape cancel, and correct insertion paint.
- [x] Apply workspace order atomically while preserving attention-sort and pin
      guards.
- [x] Cover every defect, persistence round trip, and atomicity in focused tests;
      recorded suite evidence includes 4,870 passing tests and clean lint.
- [x] Land the implementation as `e319edc9` and strictly validate this change.

## Validation boundary

The proposed raw-SGR muse scenario was blocked by unrelated obsolete snapshot
drift and was not delivered. It is not required or claimed for the accepted
pure rule and handler fix; no standalone historical `just ci` result is claimed.
