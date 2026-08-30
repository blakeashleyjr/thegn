# THE-55 revision 1 completion

Fixed every finding in the architect revision verdict.

- Added a no-create, no-migrate, no-prune read-only SQLite opener. Dry-runs use
  it for both profiles and treat an absent target DB as an empty target.
- Captured the pre-reroot XDG state root so a named source correctly resolves a
  custom default-profile database.
- Added a stable opaque-payload warning to both human and JSON dry-run audits;
  payload contents remain excluded.
- Synchronized the migration OpenSpec delta with the implemented row scope,
  sidebar/pin policy, dispatch remapping, target-first resume behavior, source
  cleanup, liveness, notification, and credential boundary. Marked only
  completed tasks in tasks.md.
- Added a host-local MigrationControl seam backed by the production
  ControlClient, plus deterministic coverage for live refusal, kill/relist,
  unreachable control, target-before-source ordering, read-back failure,
  cleanup failure, notification warning, and human/JSON audit behavior.

## Commits

- 840f9374 fix(the-55): make dry-run database access read-only (revision 1)
- 665f051d fix(the-55): preserve custom default state root (revision 1)
- 5de8c653 fix(the-55): warn about opaque dry-run payloads (revision 1)
- c6af31e4 fix(the-55): synchronize the migration OpenSpec (revision 1)
- b2c05eae fix(the-55): add host migration control seam coverage (revision 1)

## Verification

- just quick thegn-host — passed.
- cargo clippy -p thegn-host --tests -- -D warnings — passed.
- Targeted host nextest session_move — 11 passed.
- Targeted core nextest session_migration — 8 passed.
- Targeted core read-only DB regression — 1 passed.
- nix run .#openspec -- validate --all --strict — 170 passed.
- Pre-commit treefmt hook — passed on the final staged commits.

## Unverified

- Standalone treefmt was not runnable in the shell because shfmt was absent
  from PATH; the repository pre-commit treefmt hook passed after formatting.
- Full workspace gates (just test, just lint, just ci, coverage, smoke,
  rustdoc, and e2e) were not run per the revision dev-loop policy.
- No migration or binary was run against a live state DB.
