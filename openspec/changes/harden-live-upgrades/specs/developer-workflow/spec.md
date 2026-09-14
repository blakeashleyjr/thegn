## ADDED Requirements

### Requirement: Live upgrades preserve authority and recovery

The live developer launcher SHALL stage a build before replacing the installed
executable, require explicit installation confirmation, retain a database backup
and any previous executable, serialize cooperating upgrades sharing either state
or installation, and preserve the configured migration authority.
It SHALL NOT kill processes using a name pattern or unverified PID, merge queued
work, push, or automatically restore an older database.

#### Scenario: Old controller or ambiguous inspection

- **WHEN** a same-user process is observed using the selected executable or
  database, or required inspection cannot establish the supported conditions
- **THEN** installation stops with manual shutdown guidance and no signal

#### Scenario: Build or backup fails

- **WHEN** the staged build or pre-install backup fails
- **THEN** the installed executable remains unchanged and recovery information is
  reported without an automatic database downgrade

#### Scenario: Installation and startup

- **WHEN** the supported checks and explicit confirmation succeed
- **THEN** the executable is replaced atomically and normal controller startup
  retains responsibility for migrations under the existing policy
- **AND** process launch alone is not represented as application readiness

### Requirement: Planning and build provenance

The live launcher SHALL offer a nonmutating plan and isolate both final and
intermediate Cargo outputs. Recipe arguments SHALL be passed as data, not
interpolated into shell source.

#### Scenario: Ambient shared Cargo output

- **WHEN** a shared Cargo output directory is configured externally
- **THEN** the live build uses fresh isolated final and intermediate directories
  and installs only the artifact from that selected build
