# THE-55 revision 3 completion

## Implemented

- Fixed the remaining architect finding by adding hermetic `test/smoke.sh`
  coverage for:
  - cold session moves, including source cleanup, target import, daemon-id
    clearing, and unchanged Git refs/status/object counts;
  - `session move --kill` with an isolated controllable daemon fixture;
  - group, sidebar/UI, and running-pin target collisions, asserting failure
    before either store changes;
  - human and JSON `--dry-run` against both absent and existing stale target
    databases, including opaque-payload redaction and no target DB/WAL/journal
    or schema changes.
- Marked OpenSpec task 3.4 complete in
  `openspec/changes/add-session-profile-migration/tasks.md`.

## Disputed

None.

## Verification

- `bash -n test/smoke.sh` — passed.
- ShellCheck — passed.
- treefmt with `--fail-on-change` — passed.
- `git diff --check` — passed.
- Commit hook — passed ShellCheck and treefmt.
- A targeted fixture run using the pre-existing debug binary passed the cold
  move, `--kill`, and group/pin collision paths.

## Unverified

- The debug binary was stale relative to the checked-in dry-run warning and
  read-only target-store implementation, so the dry-run assertions could not
  be validated end-to-end until a rebuild. The sidebar/UI collision was also
  not re-run with a rebuilt binary after its fixture was tightened.
- `just quick thegn-host` and a targeted `cargo build -p thegn-host` were
  started with isolated runtime/cache paths and stopped before ten minutes
  after environment/sccache restrictions; neither completed a fresh binary.
- Full workspace tests, coverage, e2e, and live-state DB execution were not
  run, per the revision dev-loop policy.
