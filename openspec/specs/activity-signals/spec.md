# activity-signals Specification

## Purpose

The sidebar's per-worktree activity dots and the signal model behind them: what "running", "idle", "blocked" and "finished" mean, how they are derived (live process set + CPU, spawn argv vs foreground probe, tier-derived states), and the grace/cadence rules that keep a dot honest instead of flickering. The contract exists so a dot never claims activity the process table can't back up.

## Requirements

### Requirement: A needs-attention dot requires a real agent

A worktree's activity state SHALL only reach a needs-attention state
(`waiting`/`read`) when a real agent is bound to or observed running in it. A
worktree positively known to carry no agent — a plain terminal, whose stored
agent is a `shell`/`local` placeholder and in which no agent process is observed
— MUST return to the settled no-dot state when it goes quiet, and a
needs-attention state it somehow holds MUST self-heal to settled.

Evidence of an agent SHALL come from either the stored per-worktree agent (with
tool drawers excluded) or a live observation that an agent program is running in
one of the worktree's panes, so an agent the user started by hand still raises
its alert. A worktree with no evidence either way MUST retain the pre-existing
behaviour rather than being treated as agent-less.

Busy-ness itself MUST NOT be agent-gated: a worktree burning CPU is reported
busy regardless, because lifecycle and hibernation decisions read that state.

#### Scenario: A plain terminal goes quiet

- **WHEN** a worktree with no agent runs a command, goes busy, and then falls
  quiet past the grace
- **THEN** its dot shows working while the command runs and returns to no dot,
  and never shows a needs-attention state

#### Scenario: An agent started by hand still alerts

- **WHEN** an agent is started by hand in a shell pane of a worktree whose stored
  agent is a placeholder, and it later goes quiet
- **THEN** the worktree is treated as agent-bearing and its dot reaches the
  needs-attention state

#### Scenario: An unclassified worktree keeps its dot

- **WHEN** a worktree is absent from the agent evidence altogether
- **THEN** its activity transitions are unchanged by this rule

### Requirement: Arming a needs-attention dot requires confirmed quiet

A worktree SHALL leave the working state for a needs-attention state only after
at least two consecutive non-busy observations **and** the configured quiet
grace measured from the start of that quiet streak. A single non-busy
observation MUST NOT arm the dot, however long the interval it covered, and any
busy observation MUST restart the streak.

The quiet timestamp exposed to consumers SHALL be the start of the quiet streak
rather than the moment of the transition, so ranking and acknowledgement see an
honest "waiting since".

#### Scenario: One quiet observation mid-turn

- **WHEN** a working worktree reports a single non-busy observation covering an
  interval longer than the quiet grace
- **THEN** its dot stays in the working state

#### Scenario: A busy blip restarts the streak

- **WHEN** a worktree reports quiet, then busy, then quiet again
- **THEN** the grace is measured from the later quiet observation

### Requirement: CPU is attributed per process

The activity scan SHALL compare CPU counters per process between samples. A
process observed for the first time contributes no advance and only establishes
a baseline; a process that has disappeared contributes nothing rather than a
negative or saturated delta; and a counter that has decreased for a known
process identifier MUST be treated as reuse and re-baselined.

#### Scenario: A short-lived command is invisible

- **WHEN** a command starts and exits between two samples
- **THEN** it contributes no CPU advance and cannot by itself mark the worktree
  busy

#### Scenario: A child exits mid-work

- **WHEN** a busy child process exits while another process under the worktree
  keeps working
- **THEN** the surviving process's advance is still reported, with no false idle
  window

### Requirement: Solicited repaints are not agent output

Output a pane produces in direct response to a user or transport action SHALL
NOT count as unsolicited agent activity. A real geometry change and a reattach
that replays scrollback MUST both mark the pane's ensuing output solicited, as
keystroke echo already is.

#### Scenario: Toggling the sidebar

- **WHEN** a layout change resizes panes and full-screen programs redraw
- **THEN** the redraw does not mark their worktrees busy

#### Scenario: A transient reconnect

- **WHEN** a pane's session drops and reattaches, replaying scrollback
- **THEN** the replayed burst does not count as agent output

### Requirement: Activity thresholds are configurable

The busy threshold, the quiet and resume graces, the spawn and echo suppression
windows, the output freshness floor, the agent gate, and the set of recognized
agent program names SHALL be configurable, with defaults that preserve the
documented behaviour.

#### Scenario: Tuning the quiet grace

- **WHEN** a user sets a different quiet grace
- **THEN** the needs-attention transition uses it, still subject to the
  confirming observation

#### Scenario: Nonsensical values

- **WHEN** a configured grace or threshold is zero or negative where a positive
  window is required
- **THEN** it is clamped to a usable minimum rather than collapsing the state
  machine

### Requirement: An explicit OSC attention signal is live state, not an inbox event

A raised hand from `OSC 9` / `OSC 777;notify` SHALL be recorded as live
per-session state and MUST NOT append a notification to the inbox by default. It
MUST be lowered when the user's input reaches the process, when the session ends,
and when the worktree's needs-you signal is acknowledged or cleared. It MUST
raise the same blocked demand an explicit `agent_attention` notification raises,
through the same attention reason, so no surface distinguishes them. A signal
from a session bound to no worktree SHALL record nothing, because an
unattributed hand can light no sidebar row. Recording an additional inbox row
SHALL be opt-in (`[notifications] agent_attention_inbox`, default off), and when
enabled MUST hold at most one current row per session rather than one per signal.

#### Scenario: A raised hand marks the worktree needs-you and leaves the inbox empty

- **WHEN** a process in a worktree's pane emits `OSC 9` with the default
  configuration
- **THEN** the worktree reaches the blocked needs-you state through the same
  reason an `agent_attention` notification uses — the sidebar dot, the `✋` chip
  and the needs-you ring all count it — and no notification row is written

#### Scenario: Answering the agent lowers the hand

- **WHEN** the user types into a pane whose process had raised a hand
- **THEN** the live per-session state is deleted, the worktree stops being
  blocked on the next hydration, and no inbox row was created or marked read in
  the process

#### Scenario: A deliberate push still records an inbox row

- **WHEN** an `agent_attention` notification is pushed deliberately
  (`thegn notify push --urgency alert`, `notify.push` over the control API, or
  the MCP tool)
- **THEN** it is recorded in the inbox and scores as it always has: this
  requirement changes the ambient OSC path only

#### Scenario: The opt-in holds one current row per session

- **WHEN** `[notifications] agent_attention_inbox = true` and one session raises
  its hand repeatedly across several turns
- **THEN** the inbox holds exactly one current `agent_attention` row for that
  session, replaced each time, rather than one row per signal

#### Scenario: A signal from a session with no worktree records no row

- **WHEN** a session that is not bound to a worktree emits an attention signal
- **THEN** no per-session state row and no notification row is written, and the
  live session feed state is unchanged
