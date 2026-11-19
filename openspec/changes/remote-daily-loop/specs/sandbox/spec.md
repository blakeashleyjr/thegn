# Sandbox

## ADDED Requirements

### Requirement: Adopt with confirmation

`thegn env adopt <sprite> --env <S>` SHALL read the sprite, print its name,
status, API `created_at`, the controller id and the account fingerprint, and
write a `ready` record only after the user types the sprite name (tty) or
passes `--yes`. It MUST refuse a missing sprite, a record owned by another
controller, account or provider, and a full `max_instances`. It MUST NOT touch
the sprite.

#### Scenario: Foreign record

- **WHEN** the sprite already has a record under another controller
- **THEN** adopt refuses and writes nothing

### Requirement: Opt-in reaper

With `lifetime_reap = true` and `max_lifetime_secs > 0`, a running client SHALL
destroy only recorded sprites older than the lifetime whose API status is not
`running`, after a complete listing. It MUST NOT create, resume or exec, and
MUST do no I/O when no env opts in.

#### Scenario: Partial listing

- **WHEN** the org listing is partial or unknown
- **THEN** nothing is deleted

### Requirement: Land local then release

`thegn land` on a provider worktree SHALL pull, land the local branch, and keep
the sandbox unless `--release-sandbox` is given. A release failure MUST be a
warning after "landed", not an error.

#### Scenario: Diverged pull

- **WHEN** the pull is diverged or dirty
- **THEN** land refuses and names the kept ref

### Requirement: Pull single-flight

Pulls of one worktree SHALL be serialised across processes by an exclusive file
lock with a 120 second wait; the waiter re-reads the local tip afterwards.

#### Scenario: Concurrent pulls

- **WHEN** a second pull starts while the first runs
- **THEN** it waits and then reports `up to date`

### Requirement: Credential custody

Sprites auth uploads SHALL use mode 0600 where the file API has a mode.
`login_homes` (user config only) MUST make Sprites copy and resync and Vercel
`upload` read the named directory instead of the live host login.

#### Scenario: Dedicated login

- **WHEN** `login_homes.claude` is set
- **THEN** the claude login is read from that directory

### Requirement: Conformance slice 2

`provider_conformance::suite` SHALL also run against Fly and VPS fakes.

#### Scenario: Fly fake

- **WHEN** the suite runs on the Fly fake
- **THEN** every case passes
