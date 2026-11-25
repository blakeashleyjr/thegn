# Control plane

## ADDED Requirements

### Requirement: Relaunch restores each recorded daemon session once

When the compositor restores tabs that hold persisted daemon-session
records, each session a live-leaf record names SHALL warm-reattach onto its
recorded leaf through the daemon-backed pane path and SHALL NOT also be
adopted as an additional pane of the same restore. The attach pool the
drain plans from a worktree's live sessions SHALL exclude every session a
live-leaf record names; a session whose record's leaf no longer exists in
its tab's tree SHALL stay adoptable.

#### Scenario: A single-pane tab relaunches single-pane

- **WHEN** thegn is relaunched after a quit detached a single-pane tab whose
  daemon session survived, and the worktree probe returns that session
- **THEN** the tab restores one pane — the reattached session — with no
  second pane and no new split

#### Scenario: A stale record's session stays adoptable

- **WHEN** a persisted daemon record's leaf no longer exists in its tab's
  tree and its session is still live
- **THEN** the session remains adoptable as a headless target

#### Scenario: A recordless live session still lands

- **WHEN** the worktree holds a live daemon session no persisted record
  names
- **THEN** the attach plan still takes the tab's missing leaf with it

#### Scenario: A neighboring tab's recorded session is left to its own tab

- **WHEN** a batch for one tab plans attach targets while another tab of
  the session holds a live-leaf record naming one of those sessions
- **THEN** that session is excluded from the batch's pool and reattaches
  when its own tab materializes
