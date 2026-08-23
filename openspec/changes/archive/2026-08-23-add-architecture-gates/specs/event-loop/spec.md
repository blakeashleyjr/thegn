## ADDED Requirements

### Requirement: The poll-timeout decision is pure and gated

The loop's `poll_input` timeout SHALL be computed by a pure function `idle_poll::poll_timeout(defer, dirty, pending_input, budget_exhausted)` with unit tests pinning `None` for an idle loop and the short batching timeout only while work is in hand, and `just lint` SHALL assert the loop has exactly one `poll_input` call site that consumes it (besides the sanctioned zero-timeout drains).

#### Scenario: Idle never polls

- **WHEN** `poll_timeout` is called with no deferred work, not dirty, no pending input and budget not exhausted
- **THEN** it returns `None`

#### Scenario: A second timed poll is added

- **WHEN** a new `poll_input(Some(Duration::from_millis(…)))` appears outside the gated site
- **THEN** `just lint` fails
