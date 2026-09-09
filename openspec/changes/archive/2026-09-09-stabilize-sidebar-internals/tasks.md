# Tasks — stabilize sidebar internals

- [x] Reconcile stale tab-nesting claims and preserve live/dormant render parity.
- [x] Extract sidebar view and key/persistence handlers under shrink-only
      ratchets.
- [x] Remove dead row state, make Manual the default sort, and clean/prune
      persisted UI tombstones and removed-object prefixes.
- [x] Route sidebar glyphs through width-one capability tokens with ASCII
      fallback and unify merge-queue status glyphs.
- [x] Preserve identity in rail mode and add configurable terminals-section
      visibility.
- [x] Cover state cleanup, sorting, glyph degradation, rail identity, and
      visibility with focused tests.
- [x] Run the recorded formatting, lint, build, test, smoke, coverage,
      documentation, cross, dependency, semantic-e2e, and strict OpenSpec gates.
- [x] Land the release overhaul in `3d9bb40f` with the deterministic semantic
      muse follow-up `7152b523`.

## Validation boundary

A separate historical manual `TERM=linux` walkthrough was not recorded and is
not claimed. Automated ASCII purity/render coverage is the accepted evidence.
