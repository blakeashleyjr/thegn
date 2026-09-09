# activity-signals

## ADDED Requirements

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
