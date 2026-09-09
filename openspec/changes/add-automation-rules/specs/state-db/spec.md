# State DB

## ADDED Requirements

### Requirement: Automation ledger and audit outcomes are transactional

The state DB SHALL persist per-rule enabled override, last-fire/recent-fire
windows, per-action fire windows, and bounded once keys, plus automation run
rows containing event/action summaries and terminal outcome. Admission SHALL
atomically update ledger state and create its run row; outcome completion and
bounded per-rule retention SHALL be durable. Audit summaries MUST be bounded
and MUST NOT persist control credentials.

#### Scenario: Racing admission executes once

- **WHEN** two processes admit the same once-per-key event concurrently
- **THEN** one transaction plans it and the other records/skips it without
  duplicate action execution

#### Scenario: Retention is bounded

- **WHEN** a rule exceeds its configured retained run count
- **THEN** the oldest rows for that rule are removed while current ledger state
  remains intact
