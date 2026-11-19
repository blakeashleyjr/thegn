# Sandbox

## ADDED Requirements

### Requirement: Pull-back survives restarts

thegn SHALL journal every successful `data = "sync"` upload, and close or
`wt rm` in any process MUST pull back using the in-memory record or else the
journal, keeping the sandbox when the pull does not succeed.

#### Scenario: Journal survives restart

- **WHEN** the UI restarts after a sync upload
- **THEN** closing the worktree pulls back using the journal entry

#### Scenario: CLI rm pulls first

- **WHEN** `thegn wt rm` runs in a fresh process
- **THEN** it pulls back before destroying and keeps the sandbox on failure

#### Scenario: Another process is pulling

- **WHEN** a `pulling` entry is younger than 15 minutes
- **THEN** the command refuses and keeps the sandbox

#### Scenario: Unreadable or mismatched entry

- **WHEN** an entry cannot be read or its target no longer matches the env
- **THEN** the command refuses, keeps the entry and names the file

### Requirement: Exec routes to the owning account

`sprite-exec` MUST use the Sprites env whose provider account matches the
sprite's ledger record, and MUST refuse, naming the candidates, when the owner
cannot be determined among several envs.

#### Scenario: Ambiguous account refused

- **WHEN** there is no record and more than one Sprites env
- **THEN** `sprite-exec` refuses and lists the candidates

### Requirement: Failed exec shows stderr

A Sprites one-shot exec with a non-zero exit SHALL append the last 8 KiB of
stderr, control characters removed, after the stdout.

#### Scenario: Stderr on failure

- **WHEN** a provision step exits non-zero
- **THEN** the output ends with the stderr tail; a zero exit is unchanged

### Requirement: Merge provenance

Integrate SHALL record the gated source tip and the target base, a ready land
the admitted source, and a manual land none.

#### Scenario: Provenance recorded

- **WHEN** a branch is integrated
- **THEN** the event row carries the source tip and target base

### Requirement: Remote runtime kept

Only a local placement on a Linux host SHALL probe the OCI runtime and
`/dev/kvm`; other placements keep the requested runtime.

#### Scenario: Remote runtime kept

- **WHEN** a remote placement requests `krun`
- **THEN** the requested runtime is kept without a local probe
