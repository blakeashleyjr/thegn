# Navigation

## ADDED Requirements

### Requirement: An issue can be turned into a worktree tab in one action

thegn SHALL provide a start action (the `s` key) on a selected issue, in both
the Issues and Mine sections of the work surface, that creates a worktree for
it — deriving its branch from the issue via a configurable session-name
template (tokens `{identifier}`/`{slug}`/`{provider}`), creating the worktree
off the event loop, and adding and focusing its tab — and records the
issue↔worktree binding in the existing link store so the panel marks it linked
and the binding survives a restart. The existing `b` (branch-from-issue) and
`D` (agent dispatch) actions MUST remain unchanged. This worktree-creation path
MUST NOT depend on the AI layer.

#### Scenario: Starting an issue creates and opens its worktree tab

- **WHEN** the user presses `s` on a selected issue with
  `auto_create_worktree` enabled
- **THEN** a worktree is created off-loop with a branch derived from the issue and
  its tab is added and focused

#### Scenario: The issue↔worktree binding is recorded

- **WHEN** an issue's worktree is created
- **THEN** the issue is recorded as linked to that worktree and shown as linked in
  the work panel, persisting across restart

#### Scenario: Worktree creation works with no agent

- **WHEN** no agent is configured
- **THEN** the start action still creates and opens the worktree tab

### Requirement: An agent can optionally be launched seeded with the issue's context

thegn SHALL, when `auto_launch_agent` is enabled and an agent is configured,
launch an agent as a visible pane in the new worktree seeded with the issue's
title, body, and URL as initial context; when the option is off or no agent is
configured, no agent is launched and the worktree-creation action still completes.

#### Scenario: Agent launches with issue context

- **WHEN** an issue's worktree is created with `auto_launch_agent` enabled and an
  agent configured
- **THEN** an agent is launched as a visible pane with the issue's title, body, and
  URL as initial context

#### Scenario: No agent launch when disabled

- **WHEN** `auto_launch_agent` is disabled
- **THEN** the worktree is created and opened with no agent launched
