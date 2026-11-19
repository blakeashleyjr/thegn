# Daemon

## ADDED Requirements

### Requirement: Producer leases

A producer kind SHALL run in the daemon only while it holds that kind's lease in
`producer_leases`, renewed each tick with a TTL of three cadences (at least 60
seconds). A UI MUST skip its own fetch while the daemon holds the lease.

#### Scenario: Daemon dies

- **WHEN** the daemon stops renewing
- **THEN** the lease lapses after its TTL and the UI resumes polling

### Requirement: Idle exit

The daemon MUST NOT idle-exit while any producer is armed. A producer that
disarms SHALL release its lease.

#### Scenario: Producer armed

- **WHEN** at least one producer is armed and the daemon would otherwise be idle
- **THEN** the daemon stays running

#### Scenario: Producer disarms

- **WHEN** a producer disarms
- **THEN** it releases its lease and, with no other producer armed, the daemon may idle-exit
