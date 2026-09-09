# State DB — authority and compatibility delta

## ADDED Requirements

### Requirement: Canonical schema migration fails closed without runtime policy

Opening the canonical shared state database SHALL require an installed migration
runtime decision before schema advancement. An absent decision SHALL return a
typed refusal and SHALL NOT migrate an existing application schema. Canonical
path aliases SHALL receive the same treatment as the configured shared path. An
explicit database path MAY initialize without shared-runtime policy only when it
is proven distinct from the canonical database.

#### Scenario: A library reaches the canonical DB without policy

- **WHEN** a process with no installed migration runtime opens the canonical DB and its schema is older than the build
- **THEN** open returns a typed policy-uninstalled refusal and the on-disk schema is unchanged

#### Scenario: A path alias names the canonical DB

- **WHEN** a caller uses an explicit lexical or filesystem alias that resolves to the canonical shared database
- **THEN** the shared migration policy still applies and the alias cannot obtain the temporary-database exemption

#### Scenario: A fresh canonical database bootstraps

- **WHEN** the canonical database has no prior application schema and migration authority is not explicitly disabled
- **THEN** it may initialize under the documented bootstrap rule without electing a production controller

#### Scenario: A hermetic test opens a temporary DB

- **WHEN** a process opens an explicit path that is not the canonical shared DB
- **THEN** the temporary DB may initialize without installing shared migration policy

### Requirement: Operations declare schema compatibility independently of authority

Every shared-DB access SHALL select either the full current-schema opener or a
typed operation from a closed compatibility catalog. Each compatibility
operation SHALL declare minimum readable/writable schema requirements and its
required tables/columns, distinct from migration authority. A client MAY use a
compatible older schema without permission to migrate it.
Compatibility access SHALL join the schema lease and SHALL NOT initialize,
migrate, prune, or advance `user_version`. If a required table, column, guard,
or side effect is unavailable, the operation SHALL refuse before issuing
incompatible SQL or return an explicit degraded result; it SHALL NOT silently
report ordinary success.

#### Scenario: A git-only operation is compatible with the older schema

- **WHEN** a client lacks migration authority but the on-disk schema satisfies the operation's declared requirements
- **THEN** the operation may use the DB without advancing its schema

#### Scenario: An undeclared compatibility operation is requested

- **WHEN** code needs older-schema access for an operation absent from the closed catalog
- **THEN** it cannot obtain a compatibility handle by supplying a string or caller-selected version and must add a reviewed declaration first

#### Scenario: A safety guard requires a newer schema

- **WHEN** an operation's remote-target guard cannot be evaluated on the on-disk schema
- **THEN** it refuses or returns an explicit degraded outcome instead of silently omitting the guard

#### Scenario: A controller races a compatible client

- **WHEN** a controller migration and a client compatibility open overlap
- **THEN** the client observes one lease-protected schema version, executes only declared-compatible operations, and never advances the schema
