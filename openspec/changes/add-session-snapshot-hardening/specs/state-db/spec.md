# State DB

## ADDED Requirements

### Requirement: Per-pane scrollback is captured on snapshot and repainted on restore

superzej SHALL capture a bounded tail of each pane's scrollback when a session is
persisted and repaint it into the pane on restore, so a resurrected pane shows its
recent history rather than a blank screen. The captured tail MUST be bounded by a
configurable cap, and a snapshot taken before this feature (with no scrollback)
MUST restore exactly as before (an empty pane), requiring a `user_version` bump
with an additive, null-defaulted column.

#### Scenario: A restored pane shows its recent history

- **WHEN** a session with a pane containing scrollback is persisted and later
  restored
- **THEN** the restored pane repaints the captured tail of its scrollback

#### Scenario: An old snapshot restores unchanged

- **WHEN** a snapshot persisted before this feature is restored
- **THEN** its panes restore with no scrollback and no error

### Requirement: Stale agent state is coerced to a settled state on restore

superzej SHALL run each persisted "running"/"active" agent or activity state
through an age-based guard at restore, downgrading any state older than a
configurable grace threshold to a settled state, so a session killed mid-run does
not resurrect a phantom forever-running indicator. States fresher than the
threshold MUST pass through unchanged, and the guard MUST run only at resurrection
without altering the live sticky-state machine.

#### Scenario: A stale running state is downgraded

- **WHEN** a session is restored whose persisted agent state was "running" and
  whose dispatch is older than the grace threshold
- **THEN** the restored state is downgraded to a settled state, not shown as
  running

#### Scenario: A fresh running state survives restore

- **WHEN** a session is restored whose persisted "running" state is newer than the
  grace threshold
- **THEN** the state is restored as running
