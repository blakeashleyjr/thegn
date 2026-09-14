## ADDED Requirements

### Requirement: Performance measurements disclose their boundaries

Rollups SHALL identify their metric version, elapsed interval and sample counts.
Submission timings MUST remain distinct from successful sink-write completion;
neither SHALL claim physical display or application-response acknowledgement.

#### Scenario: Output is queued behind a slow sink

- **WHEN** a composed frame is queued before its bytes can be written
- **THEN** submission and successful completion are measured independently
- **AND** failed writes and out-of-band output do not count as successful frames

### Requirement: Input and CPU accounting preserve observed work

The earliest pending input observation SHALL survive dispatch and coalescing.
Busy wall time SHALL include synchronous dispatch and exclude blocking poll wait.
Hydration parent and scoped-child thread CPU SHALL be distinguished without
double-counting parent-thread spans or implying subprocess CPU coverage.

#### Scenario: Dispatch continues directly to the next iteration

- **WHEN** a synchronous handler performs work and continues the loop early
- **THEN** its active time is counted, and its earliest pending input remains

#### Scenario: Writer instrumentation is idle

- **WHEN** no existing loop event requires a rollup
- **THEN** writer measurements add no timer or success wake
