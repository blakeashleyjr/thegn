# Agent

## ADDED Requirements

### Requirement: A running compositor adopts bounded daemon-session intents

A running compositor SHALL claim `adopt_session` intents and attach each fresh,
valid, not-already-visible daemon session to its resident worktree through the
existing daemon-backed pane path. Claiming SHALL drain the selected intents so
they cannot accumulate indefinitely. Stale, malformed, duplicate, or
unreachable intents SHALL be discarded safely, and an actionable unreachable
target SHALL be reported to the user. Adoption SHALL NOT move focus unless the
intent requests focus.

#### Scenario: A fresh headless session becomes a resident pane

- **WHEN** a valid adoption intent names a live daemon session and a resident
  worktree
- **THEN** the compositor attaches one ordinary daemon-backed pane in that
  worktree without changing focus by default

#### Scenario: An old or duplicate intent is bounded

- **WHEN** an adoption intent exceeds the freshness window or names a session
  already displayed
- **THEN** the compositor claims and discards it without creating another pane

### Requirement: The dispatch roster has a standalone stage-oriented board

thegn SHALL expose the agent-dispatch roster through a standalone Pipeline
Board overlay. The board SHALL group records by configured pipeline stage and
SHALL retain unconfigured and unstaged records. At wide widths it SHALL present
stage columns; at narrow widths it SHALL present an equivalent stacked layout.
Configured stages with no current record SHALL remain visible.

Rows SHALL expose the dispatch identity and bounded operational facts available
from the roster and configuration, including status, stalled state, agent,
concurrency, issue or artifact, age, and next-stage information. Selection
SHALL be preserved by stable dispatch ID across refreshes when the selected
record remains present.

The board SHALL be read-only over dispatch state: rendering or activating it
MUST NOT advance a stage or enforce a concurrency policy.

#### Scenario: Wide and narrow layouts preserve stage meaning

- **WHEN** the same roster is rendered at wide and narrow terminal widths
- **THEN** the board uses columns and stacked groups respectively without
  dropping configured, unconfigured, or unstaged work

#### Scenario: A refresh preserves a surviving selection

- **WHEN** roster ordering changes but the selected dispatch ID remains present
- **THEN** the board keeps that dispatch selected

#### Scenario: Empty configured stages communicate pipeline shape

- **WHEN** a configured stage has no current dispatch
- **THEN** the stage remains represented with an empty-state treatment

### Requirement: Board activation uses the shared worktree path

Activating a board row SHALL navigate through the shared worktree activation
path. A resident target SHALL open directly. A dormant target that can be
materialized from persisted worktree evidence SHALL use the shared fallback.
An unreachable target SHALL surface failure instead of silently doing nothing.

#### Scenario: A dormant dispatch target can be opened

- **WHEN** the selected dispatch references a known worktree that is not
  currently resident
- **THEN** activation materializes and opens that worktree through the shared
  fallback path

### Requirement: Roster hydration adds no new wake source

Roster reads SHALL execute off the event loop and return through the existing
refresh and waker path. The compositor MAY take an initial or dirty-triggered
sample while the board is closed, but periodic sampling SHALL run only while
the board is open. An unchanged or stale result SHALL NOT request a repaint.

#### Scenario: A closed board has no polling cadence

- **WHEN** the Pipeline Board is closed and the roster is not marked dirty
- **THEN** no periodic roster read is scheduled for the board

#### Scenario: An unchanged sample does not repaint

- **WHEN** a current roster sample equals the compositor's existing roster
- **THEN** the sample produces no frame damage

### Requirement: Pipeline roster damage follows visible consumers

When the Pipeline Board is closed, a roster change that affects only derived
sidebar stage evidence SHALL produce at most a sidebar-scoped incremental
frame. When the Pipeline Board is open, a changed roster SHALL request a full
frame under the existing boxed-overlay rule. Pane output without an open
overlay SHALL remain eligible for pane-scoped incremental rendering.

#### Scenario: Closed-board stage evidence is bounded

- **WHEN** a roster refresh changes only a worktree's derived sidebar stage and
  the board is closed
- **THEN** rendering is limited to a sidebar-scoped incremental frame

#### Scenario: An open board follows the overlay rule

- **WHEN** a changed roster is delivered while the board is open
- **THEN** the compositor requests a full frame for the boxed overlay

### Requirement: Dispatch timestamps have one rendering unit

Dispatch persistence and hydration SHALL present Unix-millisecond timestamps to
age and stalled calculations. Historical plausible Unix-second values SHALL be
normalized at the persistence or read boundary before reaching board rendering.

#### Scenario: A historical seconds value has a truthful age

- **WHEN** a historical dispatch row stores its timestamp in Unix seconds
- **THEN** hydration normalizes it to milliseconds before the board computes
  age or stalled state

### Requirement: Worktrees expose derived live-stage evidence

A worktree with an active staged dispatch SHALL expose its current stage as
derived sidebar evidence. Terminal dispatches SHALL NOT leave a live-stage
claim behind. Presentation hierarchy and placement are governed by the
separate sidebar-pipeline contract.

#### Scenario: A terminal dispatch clears live-stage evidence

- **WHEN** a worktree has no non-terminal staged dispatch
- **THEN** its derived live-stage evidence is absent
