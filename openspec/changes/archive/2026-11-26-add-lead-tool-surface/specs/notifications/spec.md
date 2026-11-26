# Notifications

## ADDED Requirements

### Requirement: Approval requests are attention notifications

An approval request SHALL be stored as an `agent_attention` notification row
without a schema change. The Lead MUST NOT be able to resolve it.

#### Scenario: Lead requests approval

- **WHEN** the Lead calls `approvals.request`
- **THEN** a pending `agent_attention` notification exists and `approvals.list`
  shows it

### Requirement: Notice frames are opt-in

The event stream SHALL deliver `Notice` frames only to subscribers whose
`FeedFilter.kinds` names `notification`, `dispatch` or `automation_run`.

#### Scenario: Legacy subscriber

- **WHEN** a subscriber sets no kinds filter
- **THEN** it receives no `Notice` frame

### Requirement: Caller attribution is sanitized and audit-only

`THEGN_CALLER` SHALL be accepted only with characters `[A-Za-z0-9._:/@-]` and at
most 64 bytes, otherwise recorded as `invalid`, and MUST NOT influence
authorization.

#### Scenario: Hostile caller value

- **WHEN** `THEGN_CALLER` contains a space
- **THEN** the audit records `invalid`
