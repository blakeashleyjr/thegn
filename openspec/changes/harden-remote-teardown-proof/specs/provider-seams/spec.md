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

### Requirement: Bound sandbox teardown preserves work

Worktree-aware teardown paths (`wt rm`, TUI delete, `env down`, and `land --release-sandbox`) SHALL resolve the authoritative worktree binding and, when bound, try to pull its commits before destroying or recycling the sandbox. By default they SHALL refuse and retain the sandbox when the binding, pull result, or dirty-state check is unknown, or when commits remain diverged/local-dirty or the sandbox contains uncommitted or untracked paths. The refusal SHALL say that nothing was destroyed and how to recover. Loss may be accepted only by commands that expose the override: `wt rm --force`, TUI force delete, or `env down --force`. `land --release-sandbox` has no such override.

#### Scenario: Refuse unproven destroy

- **WHEN** `wt rm`, TUI delete, `env down`, or `land --release-sandbox` cannot prove the binding and pull state, or finds unpulled or dirty sandbox work
- **THEN** it keeps the sandbox and refuses to destroy it; only the supported force forms may accept the reported loss

#### Scenario: Bound sandbox deprovision refusal

- **WHEN** `env deprovision <id>` resolves the sandbox as bound to a registered worktree, or cannot resolve whether it is bound
- **THEN** it keeps the sandbox and tells the user to use the worktree-aware teardown path; `env deprovision --all` skips known-bound sandboxes and refuses if binding resolution is incomplete

#### Scenario: Environment row cannot orphan a sandbox

- **WHEN** `env rm` would remove an environment still assigned to a registered worktree with a provider sandbox
- **THEN** it refuses to remove the environment and identifies a worktree teardown or environment move as the next step

#### Scenario: Environment move retains prior sandbox

- **WHEN** `env set` moves a worktree away from a provider environment
- **THEN** it tries to pull first, retains the old sandbox, and warns if uncommitted or unpulled work may remain; it does not silently destroy that sandbox

#### Scenario: Prune keeps bound worktree

- **WHEN** `wt prune` considers a worktree bound to a provider sandbox
- **THEN** it keeps the worktree and reports the provider-sandbox reason

### Requirement: Sprites lifetime reaping preserves bound or unresolved sandboxes

The Sprites lifetime reaper and `env reap` SHALL skip sandboxes bound to registered worktrees and SHALL fail closed when binding resolution is incomplete. They SHALL use offline binding data for this decision and SHALL NOT wake a sandbox to decide whether it is bound.

#### Scenario: Lifetime reaper keeps bound sandbox

- **WHEN** a provider lifetime reaper finds a sandbox bound to a registered worktree
- **THEN** it keeps the sandbox and reports which worktree owns it

#### Scenario: Lifetime reaper fails closed on unknown binding

- **WHEN** the worktree registry or binding cannot be read completely
- **THEN** the reaper refuses destructive application or keeps every possibly bound sandbox

### Requirement: Quit confirmation

With `[ui] confirm_quit = true` the UI SHALL ask before quitting on `Ctrl-q`; a second `Ctrl-q` confirms and any other key cancels. Daemon panes survive either way.

#### Scenario: Quit confirmation

- **WHEN** the user presses `Ctrl-q` once
- **THEN** a prompt is shown and the UI stays open until `Ctrl-q` is pressed again
