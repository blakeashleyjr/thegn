# Attention Signals

## ADDED Requirements

### Requirement: A pane process can raise an explicit attention signal via OSC

A process running in a pane SHALL be able to raise an explicit attention signal
by emitting a recognized OSC escape sequence, and thegn MUST parse it at the
terminal-emulator seam into a normalized attention event without disturbing the
pane's rendered content. The recognized sequences are `OSC 9 ; <text>` (the body
is `<text>`) and `OSC 777 ; notify ; <title> ; <body>`; an `OSC 777` whose
sub-command is not `notify` MUST be ignored as an attention signal.

#### Scenario: OSC 9 raises a signal

- **WHEN** a pane process emits `OSC 9 ; Build finished ST`
- **THEN** thegn raises an attention event for that pane with body "Build
  finished" and the pane's screen content is otherwise rendered unchanged

#### Scenario: OSC 777 carries a title and body

- **WHEN** a pane process emits `OSC 777 ; notify ; Tests ; 3 failed ST`
- **THEN** thegn raises an attention event with title "Tests" and body "3
  failed"

#### Scenario: Non-notify OSC 777 is not a signal

- **WHEN** a pane process emits an `OSC 777` sequence whose sub-command is not
  `notify`
- **THEN** no attention event is raised

### Requirement: A process can raise the same signal via the notify CLI verb

thegn SHALL provide a `thegn notify` CLI verb that raises the same attention
event as the OSC path for a target worktree or pane, so a process that cannot
emit escape sequences (or a shell hook) can still raise its hand. The target MUST
resolve from `--worktree`/`--pane` flags or, when absent, from the
`$THEGN_WORKTREE`/`$THEGN_PANE` environment exported into panes; when no
live host session is running the verb MUST exit non-zero with a clear message.

#### Scenario: notify verb raises attention for the current pane

- **WHEN** a process inside a pane runs `thegn notify "ready for review"` with
  `$THEGN_PANE` set
- **THEN** an attention event is raised for that pane identical to the OSC path

#### Scenario: notify verb outside a live session fails clearly

- **WHEN** `thegn notify` runs with no live host session
- **THEN** it exits non-zero and prints that a running thegn session is
  required

### Requirement: An explicit attention signal drives the existing attention pipeline authoritatively

An attention signal SHALL flow through the existing EventBus, notification,
sidebar-badge, and activity-dot consumers, and it MUST take precedence over the
inference-based (CPU / screen-phrase) heuristics: an explicitly-raised
needs-attention state is sticky and MUST NOT be cleared by transient CPU
activity, only by the process resuming output or the human focusing the pane.

#### Scenario: Signal marks the worktree needs-attention and enqueues it

- **WHEN** an attention signal is raised for a worktree
- **THEN** the worktree's sidebar row shows the needs-attention state and the
  worktree is enqueued in the needs-attention queue reachable by the one-key jump

#### Scenario: Explicit signal survives a CPU blip

- **WHEN** a worktree has an explicitly-raised needs-attention state and its
  process briefly consumes CPU
- **THEN** the needs-attention state persists (it is not reset by the CPU blip)

#### Scenario: Resume clears the signal

- **WHEN** the signaling process resumes producing output or the human focuses
  the pane
- **THEN** the needs-attention state clears

### Requirement: The sidebar can order rows by attention (who needs the human most)

thegn SHALL provide an opt-in attention sort that ranks sidebar rows by how
much they need the human — a needs-attention (waiting) state outranks an error
state, which outranks an idle-ready state, which outranks a running state — with
an agent-raisable urgent flag (fed by the same OSC/notify signal) taking top
priority, and ties broken so the longest-waiting row ranks first. This is a sort
mode (row ORDER); it is orthogonal to notification priority (flag color) and MUST
default off so existing orderings are unchanged unless selected.

#### Scenario: A waiting row outranks a running row

- **WHEN** the attention sort is active and one worktree is waiting for the human
  while another is running
- **THEN** the waiting worktree is ordered above the running one

#### Scenario: An urgent flag floats to the top

- **WHEN** a process raises an urgent attention signal for its worktree
- **THEN** that worktree is ordered above rows without an urgent flag

#### Scenario: Longest-waiting breaks ties

- **WHEN** two worktrees share the same attention tier
- **THEN** the one that has been waiting longer is ordered first

#### Scenario: Attention sort is opt-in

- **WHEN** the attention sort has not been selected
- **THEN** the sidebar order is unchanged from the previously active sort
