# Sandbox

## ADDED Requirements

### Requirement: Record before create

thegn SHALL write a `creating` ledger record for a sprite before it calls the
Sprites create API, and a `ready` record after it succeeds.

#### Scenario: Create fails

- **WHEN** the create call fails
- **THEN** the `creating` record remains until `env reap` cleans it up

### Requirement: Proof before delete

thegn MUST delete a sprite only when this host's ledger recorded it. A sprite
with no record, including one that predates the ledger, SHALL be listed as "not
recorded by this host" and never deleted by thegn.

#### Scenario: Unrecorded sprite

- **WHEN** `env reap --provider sprites --apply` or `env down --all` meets a sprite with no record
- **THEN** it is skipped and counted, not destroyed

### Requirement: Headless hold with ceiling

thegn SHALL keep a sprite awake only for headless work, through a task that
expires in 5 minutes, refreshes every 60 seconds and stops at a ceiling of
`max_lifetime_secs` or 4 hours. Interactive panes MUST NOT hold.

#### Scenario: Crash

- **WHEN** the host dies while a held command runs
- **THEN** the hold lapses within the ceiling plus 5 minutes

### Requirement: No background sprite creates

thegn MUST NOT create a sprite in the background for an unfocused worktree.

#### Scenario: Eager workspace

- **WHEN** `eager = "workspace"` and a worktree is not focused
- **THEN** no sprite is created for it

### Requirement: Conformance suite

thegn SHALL keep a provider conformance suite (`provider_conformance.rs`, entry
point `suite`) that runs against the sprites mock and the Vercel fake.

#### Scenario: Suite runs

- **WHEN** the svc tests run
- **THEN** the suite passes for both providers
