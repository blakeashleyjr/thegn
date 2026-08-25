# profiles

## ADDED Requirements

### Requirement: A session moves between profiles cold, without its credentials

thegn SHALL move a session — the worktree group's persisted resurrection
state (`tab_groups` row, its `group_tabs` rows, and the worktree registration
if absent in the target) — from one profile's store to another via
`thegn session move <worktree> --to-profile <name>` (capability
`sessions.migrate`, `Verb::MigrateSession`, Admin scope, CLI surface only).
The move is cold by definition: live daemon sessions referenced by the group
MUST block the move, listed by id, unless `--kill` is given, in which case
they are stopped through the source profile's daemon before transfer. Nothing
environment-, token-, or identity-shaped SHALL be transferred; daemon session
references MUST be cleared in the transferred rows so panes respawn under the
target profile's own clear-then-allowlist composition. The target profile
must already exist, and a group-key collision in the target MUST abort before
any write.

#### Scenario: A cold group moves and respawns under the target identity

- **WHEN** a worktree group with no live daemon sessions is moved from
  profile `default` to profile `work`, and `work` is later launched
- **THEN** the group appears in `work` with its tabs, layout, cwds and
  scrollback restored, and its panes spawn with `work`'s composed environment
  and git identity — no variable from `default`'s composition present

#### Scenario: Live sessions block without explicit kill

- **WHEN** the group has running daemon sessions and the user omits `--kill`
- **THEN** the move is refused, the live session ids are listed, and both
  stores are unchanged

#### Scenario: --kill stops through the source daemon, then moves

- **WHEN** the user repeats the move with `--kill`
- **THEN** the source profile's daemon kills those sessions, the rows
  transfer, and the transferred rows carry no daemon session ids

#### Scenario: Collision aborts cleanly

- **WHEN** the target profile already holds a group with the same key
- **THEN** the move aborts before writing, naming the collision, and both
  stores are unchanged

#### Scenario: Credentials never ride along

- **WHEN** the transferred rows are inspected in the target store
- **THEN** they contain layout, titles, cwds, command strings and scrollback
  only — no environment variables, tokens, or identity configuration from
  the source profile

### Requirement: A cross-profile move is transactional, resumable, and visible on both sides

The transfer SHALL commit into the target store before deleting from the
source store, each side in a single transaction, so an interruption leaves
the group visible in both profiles (never in neither), and re-running the
same move SHALL detect the committed target and complete the source deletion.
`--dry-run` SHALL print the full plan — rows to transfer, live sessions that
would require `--kill`, the collision verdict, and a notice that command
strings and scrollback snapshots cross the profile boundary — without
touching either store. After a move, the source invocation SHALL report what
moved and what was killed; the group SHALL appear in the target profile at
its next launch, and when the target profile's daemon is reachable a
notification SHALL be pushed through its existing notification door
(best-effort: an unreachable target daemon is not an error).

#### Scenario: Interruption favors duplication over loss

- **WHEN** the mover crashes after the target commit but before the source
  delete
- **THEN** both profiles list the group, and re-running the move completes
  the source-side deletion without re-inserting into the target

#### Scenario: Dry-run tells the whole truth

- **WHEN** the user runs the move with `--dry-run` on a group with one live
  session
- **THEN** the output lists the rows that would transfer, names the live
  session requiring `--kill`, states the collision check result, and notes
  that scrollback/command text will cross profiles — and neither store
  changes

#### Scenario: A running target learns without restart

- **WHEN** the target profile's daemon is running at move time
- **THEN** it receives a notification that the group arrived and will appear
  at next launch
