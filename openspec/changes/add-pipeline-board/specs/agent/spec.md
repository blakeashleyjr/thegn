# Agent

## ADDED Requirements

### Requirement: A running compositor adopts daemon sessions filed for adoption

A session opened with adoption requested (`sessions.open` with `adopt`) files an
`adopt_session` intent. A running compositor SHALL claim those intents on its
model-hydration pass and graft each named daemon session into a real pane in
that session's worktree tab group, so an agent launched from outside the UI —
the control API, a supervising agent, a pipeline stage — becomes a pane the user
can watch and type into.

Adoption MUST reuse the existing daemon-session attach path (the same
`attach`-mode spawn the compositor's warm-reattach uses); it MUST NOT introduce a
second way for a daemon session to become a pane. An adopted pane is therefore
an ordinary daemon-backed pane: it persists, warm-reattaches after a restart, and
degrades to a fresh session if the attach finds a dead one.

Claiming SHALL be drain-all — every pending row is claimed and deleted in one
pass — so intents cannot accumulate unread. A claimed row that is stale (older
than the adoption freshness window), malformed, names a session a pane is already
showing, or names a worktree that is not resident in this session SHALL be
claimed and discarded rather than applied, and a discarded row that the user
could act on SHALL surface a reason rather than failing silently.

`focus` defaults to false and MUST be honoured: an adoption that does not request
focus MUST NOT move the user's focus.

#### Scenario: A stage agent launched headless becomes a live pane

- **WHEN** a session is opened with adoption requested for a worktree that is
  open in the running compositor
- **THEN** a pane attached to that daemon session appears in that worktree's
  active tab, and the user's focus does not move

#### Scenario: A fan-out yields one pane per session

- **WHEN** several sessions are opened with adoption requested before the
  compositor's next hydration pass
- **THEN** every claimed intent is applied — the rows are not collapsed to a
  last-one-wins — and one pane appears per session

#### Scenario: A backlog written while no UI was running does not erupt into panes

- **WHEN** the compositor starts and the mailbox holds adoption intents older
  than the freshness window
- **THEN** those rows are claimed and discarded without spawning panes, and the
  mailbox is left empty

#### Scenario: A session already on screen is not adopted twice

- **WHEN** an adoption intent names a daemon session a live pane is already
  showing
- **THEN** the row is discarded and no second pane is created

#### Scenario: A worktree that is not open here is reported, not ignored

- **WHEN** an adoption intent names a worktree that is not a resident group in
  this session
- **THEN** the row is discarded, the session is left headless, and the user is
  told which worktree could not be reached

### Requirement: The agent-dispatch roster is presented as a stage-grouped board

thegn SHALL present the agent-dispatch roster as a board grouped by pipeline
stage, reachable as a tab of the system-monitor overlay. The board SHALL show,
per roster row, its status (using the same status vocabulary the dispatch CLI
prints), the agent name, the worktree's basename, the originating issue, and the
age since dispatch.

Rows SHALL be grouped by stage in configured stage order; stages present on the
roster but absent from configuration SHALL follow in name order; rows with no
stage SHALL trail, grouped as unstaged. Within a stage, rows SHALL be ordered
oldest-dispatch-first, and a row chunked out of another SHALL render indented
directly beneath its parent. A row whose recorded parent is not present in its
own stage group SHALL render as a root of that group rather than being omitted.

The board SHALL be hidden when there is nothing to show — no roster row and no
configured pipeline — so an empty surface is never presented.

The board is READ-ONLY over the roster: no board interaction advances a stage,
enforces a concurrency limit, or expires a row. Stage transitions remain the
supervising agent's, written through the dispatch verbs.

#### Scenario: Stages group in configured order with unstaged rows last

- **WHEN** the roster holds rows for configured stages, for a stage absent from
  configuration, and rows with no stage
- **THEN** the board lists the configured stages in configuration order, then
  the unconfigured stage, then the unstaged rows

#### Scenario: Chunk rows indent under the row they were fanned out of

- **WHEN** a stage row records several child rows chunked out of it
- **THEN** each child renders indented beneath that row, in dispatch order

#### Scenario: An orphaned parent reference still renders its row

- **WHEN** a row records a parent that has been pruned, or that belongs to a
  different stage
- **THEN** the row still appears, as a root of its own stage group

#### Scenario: No pipeline, no tab

- **WHEN** the roster is empty and no pipeline is configured
- **THEN** the board's tab is not offered

### Requirement: Activating a board row lands its worktree

Activating a board row SHALL navigate to that dispatch's worktree using the same
activation path a sidebar row uses, so the board and the sidebar can never
disagree about where a worktree lives. When the worktree has no row to land on,
the board SHALL say so rather than doing nothing.

#### Scenario: Enter on a row jumps to the worktree

- **WHEN** the user activates a board row whose worktree is open
- **THEN** that worktree's tab is focused and the board closes

#### Scenario: An unreachable worktree is reported

- **WHEN** the user activates a row whose worktree has no sidebar row to land on
- **THEN** the board reports that there is no open worktree for it and stays put

### Requirement: The roster is hydrated off the event loop with no new wake source

The board's roster data SHALL be read off the event loop and delivered over the
existing refresh channel with a waker pulse. Periodic re-reads SHALL happen only
while the board is the live view; a closed board MUST cost no periodic read. A
roster change made outside the loop (a pane exit stamping a row) SHALL cause a
re-read without introducing a timer, a thread, or any other wake source.

A roster sample that carries no change MUST NOT repaint.

#### Scenario: A closed board costs nothing

- **WHEN** the board is not the live view
- **THEN** no periodic roster read is performed

#### Scenario: An unchanged sample does not repaint

- **WHEN** a roster sample is delivered that equals the current roster
- **THEN** no frame is painted

### Requirement: Board updates are bounded diffs, never a full chrome recompose

Updates driven by the roster SHALL be bounded-diff frames — pane- or
sidebar-scoped incremental updates — and MUST NOT force a full chrome recompose.
A test SHALL lock this: the roster feed may raise pane or sidebar damage and
nothing wider.

#### Scenario: A stage-tag change repaints only the sidebar

- **WHEN** a roster refresh changes only the sidebar's stage tags
- **THEN** the frame is a sidebar-scoped incremental update

#### Scenario: A stage agent's output stays a pane-scoped diff

- **WHEN** an adopted stage-agent pane produces output
- **THEN** the frame recomposes only that pane

### Requirement: The sidebar tags a worktree with its live pipeline stage

A worktree row SHALL carry a short tag naming the stage of that worktree's most
recent **active** roster row, rendered beside the activity dot. The tag is
evidence, not state: it MUST NOT introduce an activity state, and a worktree
whose live roster row is parked for a human MUST reach the existing
"blocked / needs you" evidence rather than a stage-specific signal.

A worktree with no live staged dispatch SHALL carry no tag, and a terminal
(finished, failed, merged, abandoned) row MUST NOT keep tagging its worktree.

#### Scenario: A worktree running a stage shows that stage

- **WHEN** a worktree's newest active roster row is at a named stage
- **THEN** its sidebar row shows that stage beside its activity dot

#### Scenario: A finished stage stops tagging

- **WHEN** a worktree's only roster rows are terminal
- **THEN** its sidebar row shows no stage tag

#### Scenario: A stage parked on a human reads as blocked

- **WHEN** a worktree's active roster row is waiting on a human
- **THEN** the worktree scores into the existing blocked attention tier, with no
  new activity state or notification kind involved
