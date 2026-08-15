# Agent

## Purpose

thegn's agent surface is a thin, config-driven launcher: coding agents are
external CLIs the user declares in config, launched as ordinary pane processes
inside the worktree's sandbox boundary. thegn remembers which agent a worktree
runs, carries agent logins into sandboxes, and folds agent output into the
per-worktree activity signal. There is no embedded agent harness and no model
traffic routing — the AI/agent layer (LLM proxy, ACP harness, managed pi) was
removed from the codebase before the public alpha; the shell is AI-free and any
future AI layer must be strictly additive.

## Requirements

### Requirement: Agents and tools are config-driven argv launchers

thegn SHALL let users declare coding agents (`[[agents]]`) and per-worktree
tools (`[[tools]]`) in config as named argv commands, and the picker SHALL
offer every configured agent, then every tool, then a literal `shell` entry
(the `__shell__` sentinel resolves to a login shell). Launching an entry MUST
compose its sandbox-wrapped argv + env and spawn it as the pane's own process.

#### Scenario: A configured agent launches as the pane process

- **WHEN** a user picks a configured `[[agents]]` entry for a worktree
- **THEN** its command is resolved to a sandbox-wrapped argv and spawned as
  that pane's own process

#### Scenario: The shell sentinel launches a plain shell

- **WHEN** a user picks the `shell` entry (or an agent whose command is
  `__shell__`)
- **THEN** the pane runs a plain login shell rather than an agent

### Requirement: The worktree remembers its agent

thegn SHALL record which agent a worktree runs (the DB `worktrees.agent`
column) so the choice survives across sessions and attributes per-worktree
state: session resurrection relaunches the remembered agent, the sidebar agent
marker reflects it, and activity signals distinguish agent-bearing worktrees
from plain shell/tool panes.

#### Scenario: The agent choice survives a restart

- **WHEN** a worktree was created with a configured agent and thegn restarts
- **THEN** the worktree's remembered agent is restored from `worktrees.agent`
  and used for resurrection and attribution

### Requirement: Agent logins sync into sandboxes

When a worktree's interactive process runs in a provider sandbox, thegn SHALL
upload the relevant agents' host config/credential files so the agent is
logged in there. Auth-critical files (a small explicit allowlist) MUST always
be uploaded first without a budget check; the remaining config tree is
uploaded best-effort under a time budget with bounded concurrency, and
executable bits MUST be preserved.

#### Scenario: Auth-critical files guarantee a usable agent

- **WHEN** an agent's login sync runs against a slow provider and the full
  config tree cannot finish within the budget
- **THEN** the auth-critical allowlist has already been uploaded, so the agent
  is authenticated and usable; only non-critical extras may be missing

### Requirement: Agent output feeds the activity signal

Unsolicited PTY output from panes of an agent-bearing worktree SHALL count as
a busy signal for the activity state machine, so an agent that is working but
using ~0% CPU (blocked on a model response, redrawing a spinner) is not marked
waiting mid-turn. Output within the solicited-echo gap after user input,
output from freshly spawned panes (spawn grace), and panes whose spawn program
is a shell or a configured tool MUST NOT count.

#### Scenario: A spinner keeps the agent working

- **WHEN** an agent-bearing worktree's pane keeps emitting unsolicited output
  with no user keystrokes
- **THEN** the worktree's activity stays busy rather than flipping to waiting

#### Scenario: A quiet agent flips to waiting

- **WHEN** the agent stops emitting output and its CPU signal is quiet past
  the grace period
- **THEN** the worktree's activity flips to waiting (unread)

### Requirement: Sandboxes provision the configured agents

The `[[agents]]` list SHALL be the source of truth for which coding-agent CLIs
a sandbox provisions (install + login carry), deduplicated by agent kind
(`provider`, else the command's program basename). The `[sandbox.home] agents`
list MUST override it with an explicit install list, and only when no
`[[agents]]` are configured at all does thegn fall back to detecting the
host's agents.

#### Scenario: An explicit install list overrides the picker set

- **WHEN** `[sandbox.home] agents = ["claude"]` is set alongside several
  `[[agents]]` entries
- **THEN** the sandbox installs and carries login for `claude` only

### Requirement: The agent layer is strictly additive

The shell SHALL function fully with no agent configured; agent features MUST
NOT be a hard dependency of the AI-free shell.

#### Scenario: No agent configured

- **WHEN** no agent is configured
- **THEN** the shell operates normally with agent features simply unavailable
