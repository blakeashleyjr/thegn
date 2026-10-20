## ADDED Requirements

### Requirement: Preserve exact drain outcome metadata

Drain status transitions SHALL distinguish result OIDs, actual conflict paths
and diagnostics, clearing superseded fields through an explicit replacement
contract. Existing partial-update store semantics SHALL remain unchanged.

#### Scenario: Infrastructure error after a conflict

- **WHEN** a later attempt is held by an infrastructure or canonical-history error
- **THEN** its diagnostic is stored in error_detail and stale conflict paths are cleared
- **AND** no speculative successful result OID is retained

#### Scenario: Progress reaches the panel

- **WHEN** a drain publishes progress with exact metadata
- **THEN** the panel applies those fields without interpreting human prose as paths or OIDs
- **AND** a gate-error-only result is reported as held or failed, not successful emptiness

### Requirement: Isolation refusal stops the current retry

A refused agent isolation admission SHALL terminate the current item's attempt
loop without consuming agent attempts or dispatching an agent.

#### Scenario: Required isolation remains unavailable

- **WHEN** admission returns an infrastructure hold
- **THEN** the drain reports one held/deferred result for that item and returns from its retry loop
- **AND** it neither immediately repeats admission nor blames the branch

#### Scenario: Hold permits later independent drain and next item

- **WHEN** a conflict or red-gate branch receives an isolation infrastructure hold in a multi-item drain
- **THEN** that item stops once without spending its remediation budget and the next selected item is processed
- **AND** a later independent drain can re-enumerate the held row and proceed after admission recovers

#### Scenario: Admitted agent makes no source change

- **WHEN** an admitted fixing-agent attempt returns without repairing the branch
- **THEN** immediate refolding consumes the already-incremented finite local attempt budget
- **AND** no advisory agent exit result refunds attempts or creates an unbudgeted immediate retry
