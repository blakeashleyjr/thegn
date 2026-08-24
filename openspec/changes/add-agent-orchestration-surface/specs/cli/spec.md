# CLI

## ADDED Requirements

### Requirement: Agent orchestration is drivable from the CLI

thegn SHALL expose the orchestration loop headlessly: `session open` launches a
configured agent by name into a worktree (prompt, headless/interactive, and
worktree-binding flags mirroring the control-plane launch), `wt new
--from-issue <id>` creates and links a worktree from a tracker issue,
`dispatch list` and `dispatch set-status` read and advance the durable roster,
and `issue list` accepts status and limit filters. List-shaped reads MUST emit
`--json` through the one-emitter convention under the documented exit-code
contract, so a supervisor can drive the whole loop with no MCP transport.

#### Scenario: Opening a worker headlessly

- **WHEN** `thegn session open --agent claude --prompt <p> --worktree <w>
--headless` runs against the daemon
- **THEN** the agent launches through the same composition as a TUI launch and
  the session id is printed (JSON when requested)

#### Scenario: The roster is scriptable

- **WHEN** `thegn dispatch list --json` runs
- **THEN** every dispatch row is emitted with its issue, worktree, agent, and
  parseable status

#### Scenario: Filtering issues for the next batch

- **WHEN** `thegn issue list --status todo --limit 3 --json` runs
- **THEN** at most three issues with the requested status are emitted,
  machine-readable
