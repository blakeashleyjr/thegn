# State DB

## ADDED Requirements

### Requirement: Automation audit and throttle state are persisted

The state DB SHALL carry an `automation_runs` table (rule name, trigger kind,
event summary, action kind, rendered action, outcome, timestamps) recording
every automation fire, drop, skip, failure, and dry-run with bounded
retention, and an `automation_state` table (per-rule enabled override,
last-fired stamp, hourly counter) shared by every evaluating process. Adding
the tables SHALL bump `user_version` with an additive migration; the tables
are a cache/audit layer — losing them disables no configured rule.

#### Scenario: Migration is additive

- **WHEN** an existing database opens under the new schema version
- **THEN** both tables are created by the migration and all existing data is
  preserved

#### Scenario: Retention bounds the audit log

- **WHEN** a rule accumulates more audit rows than the retention bound
- **THEN** the oldest rows for that rule are swept and the table stays
  bounded
