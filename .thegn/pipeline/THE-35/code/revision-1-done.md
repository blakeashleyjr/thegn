# THE-35 code revision 1

Implemented both architect-review findings:

- Added `notify::record_once`, which performs the core route decision, atomically
  deduplicates with `put_notification_once`, and emits sound/toast/push only
  after a new row is inserted.
- Added the global hydration wrapper with the existing durable-only fallback
  when no live `NotifyState` exists.
- Migrated GitHub `mentioned` polling and tracker `overdue` re-derivation to
  the emit-once route.
- Added a focused host regression test covering first-vs-duplicate mention and
  overdue sounds, plus drop, DND, and focused-worktree suppression.

Commits:

- `fab88047` — `fix(the-35): route mentioned hydration notifications once (revision 1)`
- `01c1df68` — `fix(the-35): route overdue hydration notifications once (revision 1)`
- `64341ee3` — `fix(the-35): keep emit-once funnel clippy-clean (revision 1)`

Verification:

- `RUSTC_WRAPPER= cargo nextest run -p thegn-host hydration_emit_once_sounds_mentions_and_overdue_only_on_insert` — passed.
- `JUST_TEMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-host` — passed.
- `RUSTC_WRAPPER= cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- Pre-commit treefmt hook — passed.

Unverified:

- Standalone `treefmt --ci` could not initialize because `taplo` is absent from
  the current PATH; `nix develop --command treefmt --ci` was blocked by a
  read-only Nix cache database. The pinned pre-commit treefmt hook passed.
- Full-workspace gates, cross-platform player execution, and e2e were not run
  per the revision dev-loop policy.
