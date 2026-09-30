## ADDED Requirements

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
