# State DB

## ADDED Requirements

### Requirement: Repo trust-on-first-use approvals are persisted

The state database SHALL record trust-on-first-use decisions for a repo's gated
sandbox requests in a `repo_trust` table (schema v32, added by the additive
migration ladder), keyed by `(repo_root, canonical request JSON)`. The canonical
request JSON is the security match key; the recorded `request_id` is a display
handle only. Reading the approved set for a repo yields the canonical request
strings whose decision is `approved`.

#### Scenario: An approval is recorded and read back

- **WHEN** a gated request is approved for a repo root
- **THEN** the repo's approved set includes that request's canonical JSON

#### Scenario: A denied request is not in the approved set

- **WHEN** a gated request is denied for a repo root
- **THEN** the repo's approved set excludes it, though the decision is listed

#### Scenario: The table is added without disturbing existing data

- **WHEN** a pre-v32 database is opened
- **THEN** the `repo_trust` table is created additively and existing rows survive
