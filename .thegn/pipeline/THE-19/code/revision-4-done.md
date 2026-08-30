# THE-19 revision 4 completion

Implemented both findings in `architect-review/revision-4.md`.

## Fixed

- Lifecycle completion application is now compositor-loop pure: successful
  worktree/workspace outcomes only reconcile in-memory session, pane, model,
  and focus state. DB/cache cleanup and layout-row removal run in the existing
  lifecycle worker before completion delivery. Live pane-state capture is no
  longer reached from `apply_completions`.
- Added a regression seam test covering every successful completion variant.
- `repo trust` now combines sandbox requests with canonical
  `hooks.<event>` requests from the lifecycle resolver. Approval uses the same
  request identity consumed by hook execution; the regression test covers
  listing, approval persistence, and subsequent hook execution.

## Disputed

None.

## Commits

- `c9f011a3` — move lifecycle completion I/O off loop
- `39cf8826` — expose lifecycle hook trust requests
- `59acf02b` — finish off-loop lifecycle cache cleanup and test coverage
- `688346b3` — satisfy scoped clippy for lifecycle completion

## Verification

- `just quick thegn-host` — passed.
- `cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- Focused nextest lifecycle completion test — passed.
- Focused nextest repo-trust hook test — passed.
- Pre-commit treefmt hook — passed on the commits above.
- `git diff --check` — passed.

## Unverified

- Direct `treefmt --fail-on-change` could not run because the configured
  `taplo` formatter is not on `PATH`; the repository pre-commit formatter did
  pass.
- No full-workspace gate, OpenSpec validation, or e2e run was performed, per
  the targeted revision dev-loop policy.
