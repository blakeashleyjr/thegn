## ADDED Requirements

### Requirement: Restoration checks absence against its attached daemon

Daemon-backed panes SHALL query the authoritative roster of the daemon used for
the failed attach before opening a fresh replacement. Query errors MUST NOT
authorize replacement, and checking absence MUST NOT start another daemon.

#### Scenario: A persisted session was reaped

- **WHEN** attachment fails and the same daemon's successful roster omits the session
- **THEN** restoration opens one fresh session and emits the existing fallback event

#### Scenario: The daemon cannot answer

- **WHEN** attachment or roster access fails without authoritative absence
- **THEN** no fresh shell is opened and recovery retains the exact session identity

### Requirement: Session recovery is bounded and cancellation-aware

After transport loss the relay SHALL retry the exact known session within a
bounded attempt and elapsed-time budget. It MUST retain bounded pending input,
respect backpressure, and stop when the pane is closed or detached.

#### Scenario: A transient outage clears

- **WHEN** two attach attempts fail and a subsequent attempt succeeds within the budget
- **THEN** the same session is reused without a fresh open and pending input is delivered once

#### Scenario: A pane closes during recovery

- **WHEN** the pane owner closes its control channel during attach or backoff
- **THEN** recovery stops promptly and close-versus-detach ownership is respected
