# devcontainer Specification

## Purpose

TBD - created by archiving change cache-demanded-devcontainer-probes. Update Purpose after archive.

## Requirements

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

### Requirement: Startup capture preserves bounded operation custody

Devcontainer startup SHALL reserve one controller-owned operation before constructing its provider, preserve stdout-null and exit-status semantics, concurrently capture stderr with a 2 MiB ceiling, and bound caller waiting by one checked ten-minute deadline. Unfinished workers, pipes and their immutable config context SHALL retain admission independently of Git and capability-probe work.

#### Scenario: A noisy or inherited startup pipe

- **WHEN** stderr exceeds its ceiling or a pipe remains open after the caller deadline
- **THEN** startup returns a held unknown outcome and retains exact unfinished custody without another startup admission or automatic resource deletion.

#### Scenario: Cancellation races with spawn

- **WHEN** cancellation wins before the final spawn claim
- **THEN** no CLI child starts; if the claim already won, any uncertain completion remains held and cannot be overwritten by success.

### Requirement: Uncertain startup effects cannot trigger automatic fallback

The actual launch decision SHALL distinguish positively not-started outcomes from possible resource effects. Only positively pre-spawn failures MAY preserve existing OCI fallback; nonzero exit, post-spawn timeout/failure and abandoned success SHALL halt automatic fallback/retry within the owning controller lifetime.

#### Scenario: Nonzero exit with a descendant-held diagnostic pipe

- **WHEN** a started CLI exits nonzero and a descendant retains stderr
- **THEN** the operation is nonreusable before the owner awaits diagnostic EOF, and the unchanged caller deadline produces a held outcome with no OCI continuation.

#### Scenario: Successful output is abandoned before publication

- **WHEN** a successful result is not committed to the session registry
- **THEN** the same fixed entry retains the snapshot and uncertain operation instead of treating result delivery as accepted completion.

#### Scenario: Registry insertion unwinds before acknowledgement

- **WHEN** a new session displaces an old session and publication unwinds immediately after insertion
- **THEN** the displaced session remains in pre-admitted custody, no provider/snapshot destructor runs on the publisher, and startup admission remains held.
