# THE-32 chunk 3 completion

Implemented git-view rendering, atomic submodule UI behavior, merge conflict
reporting, and documentation for submodule pointers.

## Delivered

- Joined typed gitlink diffs and checkout state to change rows; rendered
  caps-safe submodule markers, abbreviated old/new SHAs, direction/state, and
  bounded local summaries without a `+0/-0` diffstat.
- Added off-loop submodule scans and preview reads through the existing worker
  channels and waker, with independent degraded states and stale-data carryover.
- Kept gitlink staging atomic: whole-file stage/unstage remains available,
  while line staging and drill-in are rejected with a concise status.
- Added the toggleable, caps-safe sidebar submodule indicator while preserving
  existing render-plan behavior.
- Preserved typed submodule merge conflicts with ours/theirs SHAs and excluded
  them from driver, rerere, regeneration, and blanket staging resolution;
  propagated the detail into CLI, queue, and agent handoffs.
- Documented pointer rendering, no-fetch summaries, lifecycle/config behavior,
  conflict handling, and disk-versus-LOC boundaries.

## Verification

- `just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host changes` — 25 passed.
- `cargo nextest run -p thegn-host sidebar` — 267 passed.
- `cargo nextest run -p thegn-host integrate` — 22 passed.
- `cargo nextest run -p thegn-host gitmut` — 5 passed.
- `cargo nextest run -p thegn-host panel` — 202 passed.
- `cargo nextest run -p thegn-core fold` — 42 passed.
- Required host completion/help/catalog/assets/platform ratchets — passed.
- `git diff --check` — passed.

## Unverified

- Full-workspace gates, e2e, migration, and built-binary validation were not
  run, per the chunk dev-loop policy.

Commit: `feat(the-32): render submodule pointers and conflicts`
