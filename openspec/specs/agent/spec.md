# Agent

## Purpose

thegn embeds a coding agent that is bound to a worktree, routes its model
traffic through the LLM proxy, and services its tool requests off the event loop
within the worktree's sandbox boundary. The agent layer is strictly additive — the
AI-free shell never depends on it — and the in-shell surface is intentionally
minimal.

> Note: the concrete agent harness is in transition (the embedded termite-agent is
> being superseded by a pi fork over ACP). The requirements below capture the
> decision-stable invariants that hold across that transition; harness-specific
> details belong in their own in-flight changes.

## Requirements

### Requirement: Agent binds per worktree with a minimal surface

An embedded agent SHALL bind to a worktree with a per-worktree activity indicator and a connection lifecycle, and the in-shell surface is intentionally minimal (the activity chip) with agent edits auto-applied rather than gated, per the current product decision.

#### Scenario: Agent activity shows on its worktree

- **WHEN** an agent bound to a worktree is active
- **THEN** that worktree's activity chip reflects the agent's state

#### Scenario: Edits auto-apply

- **WHEN** the agent edits files in its worktree
- **THEN** the edits are applied without a blocking in-shell review gate

### Requirement: Agent model traffic routes through the proxy

Agent model traffic SHALL route through `tgproxy` via per-worktree virtual keys so spend and budgets attribute to the worktree.

#### Scenario: Requests carry the worktree key

- **WHEN** the agent makes a model request
- **THEN** it is routed through the proxy under the worktree's virtual key

### Requirement: Agent tool services run off the loop and scoped

The terminal/filesystem/edit services the agent requests SHALL run off the event loop and be scoped to the worktree (and its sandbox boundary), never blocking the loop or adding a polling timeout.

#### Scenario: File edit serviced off-loop

- **WHEN** the agent requests a file edit
- **THEN** it is serviced off the loop, scoped to the worktree, without blocking
  rendering

### Requirement: The agent layer is strictly additive

The shell SHALL function fully with no agent configured; agent features MUST NOT be a hard dependency of the AI-free shell.

#### Scenario: No agent configured

- **WHEN** no agent is configured
- **THEN** the shell operates normally with agent features simply unavailable

### Requirement: The managed pi is acquired through the managed-tool resolver

The managed `pi` binary under `~/.thegn/pi` SHALL be described as a
`managed-tools` spec and acquired through the shared resolver rather than a
bespoke install path. `thegn agent setup` MUST remain idempotent and preserve
its observable behavior: install/refresh the pinned pi, always re-seed the
`thegn-acp` package, register it, and record the pinned version marker.

#### Scenario: agent setup installs via the shared resolver

- **WHEN** `thegn agent setup` runs and the pinned pi is not yet current
- **THEN** the pinned pi is installed through the managed-tool resolver and the
  `thegn-acp` package is (re)seeded and registered

#### Scenario: agent setup is idempotent when already pinned

- **WHEN** `thegn agent setup` runs and the pinned pi is already at the pinned
  version
- **THEN** the binary install is skipped while the `thegn-acp` package is
  still re-seeded and registered
