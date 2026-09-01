# THE-55 Architect Revision 3

Status: REVISE

## Required correction

### 1. Add the promised hermetic smoke coverage

`openspec/changes/add-session-profile-migration/tasks.md:36` leaves task 3.4
unchecked, and `test/smoke.sh:1-7` has no `session move` invocation. The
implementation therefore has no shell-level coverage for the public operation
across two profile roots, even though the OpenSpec task explicitly requires
cold move, `--kill`, collision, and dry-run cases. Unit tests behind the host
seam do not verify CLI argument wiring, profile path selection, SQLite row
effects, or the no-write dry-run behavior of the actual binary.

Expected fix: extend `test/smoke.sh` with hermetic cases using fresh isolated
`XDG_STATE_HOME`/profile roots (never the user's live state) that exercise:

- a cold move and assertions that the selected source rows are removed, target
  rows are present, and git worktree files/objects are unchanged;
- an explicit `--kill` path through a controllable test daemon or equivalent
  deterministic fixture, asserting kill-before-import and cleared target daemon
  ids;
- a target group/UI/pin collision that exits before either store changes; and
- a `--dry-run` against an absent or stale target DB, asserting no DB, WAL,
  journal, or schema-file changes and checking both human/JSON audit output is
  redacted and includes the opaque-payload warning.

Mark task 3.4 complete only after these cases pass and keep the fixture's
profile/config/credential paths isolated. Do not add a daemon route or run the
smoke cases against a live state DB.
