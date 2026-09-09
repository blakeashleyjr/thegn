# Agent

## ADDED Requirements

### Requirement: A configured agent launches by name through the control plane

thegn SHALL let a control-plane session open name a configured agent and an
optional prompt, and resolve it through the same composition an interactive
pane gets — sandbox wrapping, credential directories, bundle/identity
environment, resource cap, and the `worktrees.agent` binding — never a
reimplementation of that path. A recognized provider id MUST resolve even when
no matching `[[agents]]` entry exists; an unknown name MUST be an error, never
a guessed command. When a prompt is given the launch defaults to the harness's
headless form (overridable), with the prompt passed under the agent-task
engine's shell-quoting contract.

#### Scenario: A daemon-launched agent equals a TUI-launched agent

- **WHEN** a caller opens a session with an agent name and a worktree
- **THEN** the spawned process has the same sandbox, credentials, and
  environment as if the agent were launched from the worktree wizard, and the
  worktree's agent binding is recorded when requested

#### Scenario: A bare provider id works without config

- **WHEN** the caller names a recognized provider id on a host whose config has
  no `[[agents]]` entries
- **THEN** the launch resolves to that provider's command rather than failing
  on the operator's naming choices

#### Scenario: An unknown agent name is refused

- **WHEN** the caller names an agent that is neither configured nor a
  recognized provider id
- **THEN** the open fails with an error naming the agent, and nothing is
  spawned

### Requirement: An issue task kind seeds dispatched workers

The agent-task engine SHALL provide an issue task kind whose prompt template
renders the issue's number, title, body, URL, branch, and worktree, with a
built-in default prompt and the engine's quoting contract, so a worker
dispatched against an issue starts with the task in its prompt rather than
only environment variables. Issue dispatch MUST resolve the configured agent
(or a named one) rather than hardcoding a vendor, and MUST keep recording the
dispatch in the roster and linking the issue to the worktree.

#### Scenario: A dispatched worker receives the rendered issue prompt

- **WHEN** an issue is dispatched to an agent in a worktree
- **THEN** the agent launches with a prompt rendered from the issue's fields
  (quoted safely), the dispatch is recorded, and the issue is linked to the
  worktree

#### Scenario: Issue content cannot escape the quoting contract

- **WHEN** an issue body contains shell metacharacters or quotes
- **THEN** the rendered command carries them as data, with no free-standing
  command fragments
