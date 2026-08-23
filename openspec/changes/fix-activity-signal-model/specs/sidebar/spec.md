# Sidebar

## MODIFIED Requirements

### Requirement: Per-row activity indication

The sidebar SHALL surface per-row activity (e.g. activity dots) driven by the
host-side activity state machine.

The indication SHALL distinguish four things: that a worktree is **working**;
that its agent has **finished** and awaits the user; that its agent is
**blocked on the user**; and whether the user has **seen** an awaiting state.
Working, finished, and blocked MUST be visually distinct from one another, and
seen-versus-unread MUST be carried on a separate axis from that distinction so
the two read independently.

Whether an awaiting worktree is blocked rather than merely finished SHALL be
taken from its attention tier, so the loud state is reserved for cases with real
evidence behind them (an agent asking for input, a queue needing a human) rather
than inferred from output.

A worktree with no agent MUST NOT show any awaiting state (see the
`activity-signals` capability).

#### Scenario: Background activity shows on its row

- **WHEN** a non-focused worktree produces activity
- **THEN** its sidebar row reflects that activity state

#### Scenario: Finished and blocked read differently

- **WHEN** one worktree's agent has finished and another's is blocked on the
  user
- **THEN** their rows show visually distinct awaiting indications

#### Scenario: Seen but still waiting

- **WHEN** the user focuses a tab whose worktree is awaiting them
- **THEN** the row's indication changes to the seen form while keeping the same
  finished-versus-blocked distinction, and is not cleared

#### Scenario: A plain terminal never demands attention

- **WHEN** a worktree with no agent goes busy and then quiet
- **THEN** its row shows the working indication and then none, never an awaiting
  one
