# State DB

## ADDED Requirements

### Requirement: PR queue rows carry a comment fingerprint and an agent override

The state database's `pr_queue` table SHALL additionally record, per entry, a
fingerprint of the unresolved review thread identities last observed and an
optional agent override (an agents/tools entry name or a full command
template). The columns SHALL be added by an additive migration with a
`user_version` bump; existing rows read as having no fingerprint and no
override.

#### Scenario: An existing database upgrades in place

- **WHEN** a database created with the original `pr_queue` shape is opened
- **THEN** the new columns exist, `user_version` is advanced, and every
  existing queue row is preserved with an empty fingerprint and no override

#### Scenario: Override and fingerprint survive a restart

- **WHEN** an entry with an agent override and a recorded fingerprint is
  reloaded after a restart
- **THEN** both are present on the row
