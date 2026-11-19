# Provider seams

## ADDED Requirements

### Requirement: Teardown is proven by inventory

A provider sandbox destroy SHALL be treated as done only after a complete inventory listing no longer contains the sandbox, polled for at most 60 seconds at 3 second intervals. A partial listing SHALL NOT count.

#### Scenario: No-token refusal

- **WHEN** `wt rm` runs for a worktree with a provider sandbox and the provider token is unset
- **THEN** the removal is refused and the message names the sandbox and the two ways out

#### Scenario: Unconfirmed destroy refusal

- **WHEN** the provider destroy returns OK but the inventory still lists the sandbox at the deadline
- **THEN** `wt rm` refuses and the pool-spare row and provisioned marker are kept

#### Scenario: Orphan refusal

- **WHEN** a worktree has a stored provider location under a non-provider env
- **THEN** `wt rm` refuses and points to `env set --worktree <wt> <local env>`

#### Scenario: Close notice

- **WHEN** a provider worktree is closed
- **THEN** one notice names the kept sandbox and how to remove it

#### Scenario: Hold truth

- **WHEN** the first keep-awake hold call fails
- **THEN** the script runs unheld with a stderr line and sends no DELETE at exit

### Requirement: Quit confirmation

With `[ui] confirm_quit = true` the UI SHALL ask before quitting on `Ctrl-q`; a second `Ctrl-q` confirms and any other key cancels. Daemon panes survive either way.

#### Scenario: Quit confirmation

- **WHEN** the user presses `Ctrl-q` once
- **THEN** a prompt is shown and the UI stays open until `Ctrl-q` is pressed again
