# time-travel

## ADDED Requirements

### Requirement: A pane's recording exports as an asciicast

thegn SHALL export the focused pane's retained time-travel recording as an
asciicast v2 file (header with the pane's geometry at the earliest retained
event, output events with times rebased to zero), triggered from the replay
overlay and a palette action, written under the per-profile recordings
directory with owner-only permissions. The export MUST be bounded by the
existing replay budget and MUST be honest about it: the destination path and
the covered timespan are reported to the user. When replay is disabled or the
ring is empty the action MUST fail with a clear message rather than writing
an empty file.

#### Scenario: Export from the replay overlay

- **WHEN** the user opens replay on a pane with retained history and invokes
  export
- **THEN** a `.cast` file is written under the recordings directory and a
  toast reports its path and the timespan it covers

#### Scenario: Replay disabled

- **WHEN** `[replay] enabled` is false and the user invokes the export action
- **THEN** no file is written and the message names the `[replay]` setting

#### Scenario: The export replays faithfully

- **WHEN** an exported cast is fed to an asciicast v2 player at the recorded
  geometry
- **THEN** it reproduces the pane's retained visual history
