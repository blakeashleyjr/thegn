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

### Requirement: Fleet down and unrecorded states

A recorded sandbox the provider does not list SHALL count as `missing` only when
this host holds its local provisioned marker, otherwise as `down`. A listed
sprite with no worktree and no local ledger record SHALL count as `unrecorded`.
Neither state SHALL turn the masthead badge amber. Budget admission MUST still
count every listed row.

#### Scenario: Stopped sandbox

- **WHEN** `env down` has run and the provider no longer lists the sandbox
- **THEN** the fleet shows it as `down` and the badge stays calm

### Requirement: Read-only unrecorded listing

`env reap --provider sprites --unrecorded` SHALL list unrecorded sprites oldest
first with status, age and 30-day usage, print one `env adopt` line per sprite
with a paste-safe name, and MUST refuse `--apply`, take no lease and destroy
nothing.

#### Scenario: Apply refused

- **WHEN** `--unrecorded --apply` is given
- **THEN** the command refuses and changes nothing

### Requirement: Git audit and Vercel stderr

Seed, pull and push-lane events SHALL be appended as JSON lines to
`$XDG_STATE_HOME/thegn/provider/git-audit.jsonl` (rotated at 1 MiB, one previous
file); a failed write MUST only warn. A nonzero Vercel exec SHALL append the last
8 KiB of stderr with control characters removed, leaving raw stdout unchanged.

#### Scenario: Audit write fails

- **WHEN** the audit file cannot be written
- **THEN** a warning is logged and the operation continues
