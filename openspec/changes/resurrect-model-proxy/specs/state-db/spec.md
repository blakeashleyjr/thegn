# State DB

## ADDED Requirements

### Requirement: Model-proxy accounting lives in new tables, never the orphaned legacy set

The state database SHALL carry a `model_proxy_requests` table (per-request
metadata: timestamps, route, backend, model, protocol, caller scope,
input/output/cache-read/cache-creation token counts, cost in USD with its
source, duration, time-to-first-byte, and outcome classification) and a
`model_proxy_budget_state` table (per-scope rolling-window anchors and
accumulators). Both SHALL be added by an additive migration with a
`user_version` bump. The pre-alpha `proxy_*` tables that may exist orphaned in
user databases MUST NOT be reused, migrated, read, or dropped — the
multi-branch shared-DB contract stands. No message content of any kind is
stored in these tables.

#### Scenario: An older database gains the tables on open

- **WHEN** a database created before the model proxy existed is opened by a
  build with this change
- **THEN** the `model_proxy_*` tables are created, `user_version` advances, and
  every pre-existing row — including any orphaned `proxy_*` table — is preserved

#### Scenario: Legacy orphans stay inert

- **WHEN** a database still carries the excised `proxy_requests` table
- **THEN** the resurrected proxy neither reads nor writes it, and its rows are
  untouched

#### Scenario: Accounting survives a restart

- **WHEN** requests have been audited and budget windows accumulated, and thegn
  restarts
- **THEN** spend history and budget state are intact
