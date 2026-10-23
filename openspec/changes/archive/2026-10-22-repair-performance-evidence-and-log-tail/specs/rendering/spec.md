## ADDED Requirements

### Requirement: Resync cost is independently identifiable

Performance accounting SHALL identify full-screen resync independently from
incremental or full composition, without changing the render decision.

#### Scenario: An incremental composition needs a drift resync

- **WHEN** an incremental frame performs the periodic full-screen resync
- **THEN** telemetry identifies that extra work without classifying it as a
  chrome recompose or asserting a cause for a slow-frame warning
