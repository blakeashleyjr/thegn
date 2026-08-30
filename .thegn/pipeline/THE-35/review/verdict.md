PASS

# THE-35 security/test/bug review

Ready for the merge queue. `main` was merged first and the full `main...HEAD`
diff was reviewed.

## Findings fixed

- Repository notification overlays now cannot retain legacy rule sound command
  overrides. This closes a repo-controlled path to the trusted `sh -c` escape
  hatch; a regression test covers the rejected command and retained safe rule.
- Sound snapshot reloads now use a generation check so a slower older reload
  cannot replace a newer configuration.
- Legacy command playback now reports spawn and non-success exit failures to
  the worker diagnostic log instead of swallowing them; a failure test covers a
  non-zero command status.

## Verification

- Scoped core/host quick checks and clippy with `-D warnings`: passed.
- Focused sound, notification, routing, overlay, attention, doctor/help,
  control-schema, platform-cfg, and completion-slot tests: passed.
- Ignored-result ratchet: clean (318 pinned); async-trait ratchet: clean
  (0 pinned).
- `treefmt --ci`: passed, 0 files changed.
- `openspec validate --all --strict`: 170/170 passed.
- No migration, live state DB, `thegn` invocation, or e2e/full-workspace suite
  was run.

## Unverified

Native playback behavior on other operating systems and the full/e2e suites
remain unverified as required by the lane scope. The repository knowledge-graph
overlay was unavailable, so review used direct source and diff inspection.

## Snapshots

No frame-affecting changes; no snapshot updates are required.
