## ADDED Requirements

### Requirement: Set-only bounded pane clipboard admission

Pane OSC 52 passthrough SHALL accept only a complete bounded validated clipboard set and SHALL reject reads before writer admission.

#### Scenario: Clipboard read or malformed set

- **WHEN** a pane emits a query, empty/default or multiple selector, empty payload, invalid base64 or oversized sequence
- **THEN** no such command SHALL reach the outer writer or replace a retained valid set

#### Scenario: Split input and backpressure

- **WHEN** a valid set spans arbitrary PTY reads or writer admission is refused
- **THEN** the pane SHALL retain bounded state, preserving the latest validated write for wake-driven retry

#### Scenario: Session generation barrier

- **WHEN** reattach, fallback or exit ends a pane generation
- **THEN** partial and unadmitted pending clipboard state SHALL be cleared without joining queued old bytes to the new generation

#### Scenario: Writer ownership

- **WHEN** a valid set is accepted by the bounded clipboard queue
- **THEN** it SHALL preserve FIFO ordering with previously accepted output and SHALL NOT be included in frame-completion metrics
