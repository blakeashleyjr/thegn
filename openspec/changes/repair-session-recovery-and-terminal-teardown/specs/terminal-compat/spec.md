## ADDED Requirements

### Requirement: Terminal teardown never panics on failed cleanup

Terminal destruction MUST attempt mode restoration and owned signal cleanup
without panicking when output, terminal attributes or console handles fail.
It MUST remain safe during an existing panic unwind.

#### Scenario: The controlling PTY hangs up

- **WHEN** terminal cleanup runs after the PTY master is closed
- **THEN** failed writes and terminal-attribute restoration do not panic or abort

#### Scenario: A healthy terminal exits

- **WHEN** cleanup runs with a healthy terminal
- **THEN** saved terminal modes are restored and owned resources are released
