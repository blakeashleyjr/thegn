## ADDED Requirements

### Requirement: Diagnostic CLI probes follow eligible demand

Model hydration SHALL avoid discovering or executing the devcontainer CLI when its existing selected-config status classifier determines that no selected config is eligible for that provider. Trust and source precedence SHALL remain authoritative.

#### Scenario: Unrelated, refused or pending worktree

- **WHEN** no devcontainer is selected, selection is disabled/invalid/ambiguous, policy cannot be honored, or any required approval is pending
- **THEN** hydration performs zero capability probes and preserves the existing truthful status.

#### Scenario: Repeated eligible demand

- **WHEN** concurrent or repeated eligible requests have matching current command inputs and a fresh cached diagnostic
- **THEN** they share at most one capability producer and reuse only that matching generation's result.

#### Scenario: Inputs change during a probe

- **WHEN** environment, cwd or executable identity changes, including a return to an earlier key after intervening demand
- **THEN** stale completion is not published and waiters reobserve their own inputs before returning a cached result.

### Requirement: Capability diagnostics retain bounded ownership

Version probes SHALL have a fixed two-second deadline and a 16 KiB bound per output stream, a single retained capability slot and a separate retained reaper from Git probes. The original Git budget and failure semantics SHALL remain intact.

#### Scenario: Hung or inherited output pipe

- **WHEN** a helper fails to finish or a descendant retains an output pipe
- **THEN** the caller receives bounded degraded status, unfinished ownership retains capability capacity, and unrelated Git capture/reaping remains available.

#### Scenario: Executable becomes a special file

- **WHEN** a selected executable is replaced with a FIFO before identity opening
- **THEN** the diagnostic refuses the nonregular identity without waiting for a FIFO writer.
