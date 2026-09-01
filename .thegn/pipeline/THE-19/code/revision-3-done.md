# THE-19 code revision 3 completion

Implemented every finding in `architect-review/verdict-3.md` / `revision-3.md`.

## Fixed

- Issue-panel dispatch and daemon/control `worktrees.create` now route failed
  `git worktree add` operations through the shared force-cleanup transaction.
  The original add error remains primary, rollback failures are appended, and
  partial adds with a branch but no checkout remove the speculative branch.
- Vanished-tab reconciliation now probes SQLite/filesystem state on a background
  worker and returns a typed `RefreshKind::VanishedTabs` completion. The loop
  only applies the already-identified pure session/pane prune and focus update;
  the duplicate active-tab loop probe was removed as well.
- Added regression coverage for partial-add rollback/error preservation and the
  worker-report-only vanished-group selection seam.

## Commits

- `e8b8463a` `fix(the-19): rollback failed issue and control creates (revision 1)`
- `4262e27e` `fix(the-19): move vanished-tab reconciliation off loop (revision 1)`
- `346a6410` `fix(the-19): route create add failures through shared rollback (revision 1)`
- `4f9ba26b` `fix(the-19): remove duplicate loop-side vanished probe (revision 1)`

## Verification

- `just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host 'worktree_lifecycle::tests'` — 4 passed.
- `cargo nextest run -p thegn-host vanished_indices_use_only_worker_reported_paths` — passed.
- `cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- Pre-commit treefmt hook — passed on the commits above.
- `git diff --check` — passed.

## Unverified

- Direct `treefmt` could not run because `taplo` is not on `PATH`; the
  pre-commit treefmt hook passed.
- Full-workspace tests, coverage, CI, and e2e were intentionally not run per
  the revision dev-loop policy.
- No live state database was opened; DB-touching rollback tests used temporary
  `XDG_STATE_HOME` values.
