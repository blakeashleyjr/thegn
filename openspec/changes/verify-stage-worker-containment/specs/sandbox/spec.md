# Sandbox — stage-worker containment verification delta

## ADDED Requirements

### Requirement: Doctor evaluates the effective stage-worker containment plan

Doctor SHALL resolve the same effective stage agent, command identity,
environment precedence, relocated provider homes, callback binary, inner
harness isolation, outer backend/profile, worktree mount, and Git-directory
topology used by production stage launch. It SHALL run only a local non-billable
probe payload and SHALL NOT invoke a coding model, provider login, forge API, or
network request. Diagnostics SHALL redact environment values and credential
contents.

The result SHALL distinguish ready/contained, unwritable worktree, unwritable
Git metadata, unavailable or stale callback binary, unavailable provider home,
allowed outside write/absent containment, unsafe inner-full-access plus
uncontained outer backend, and unsupported platform/backend. A ready/contained
result SHALL require complete dynamic evidence; host-side or mount-structure
checks alone SHALL NOT produce a pass.

#### Scenario: The resolved bwrap worker is healthy

- **WHEN** doctor resolves a Linux/bwrap stage worker and its disposable dynamic checks can write the worktree, commit through its Git dirs, execute the callback, see required provider homes, and cannot write the outside sentinel
- **THEN** doctor reports ready/contained and identifies the effective backend and configuration provenance

#### Scenario: Git metadata is read-only

- **WHEN** the probe can edit its disposable worktree but `git add` or `git commit` cannot update the linked-worktree/common Git metadata
- **THEN** doctor reports git-metadata-not-writable separately from worktree writeability and names the relevant mount remediation

#### Scenario: Callback or provider home is unavailable

- **WHEN** `THEGN_BIN` is missing/stale inside containment or a required relocated provider home is absent or mounted with the wrong access
- **THEN** doctor identifies the failed subcheck without opening credentials or launching the provider

#### Scenario: Dynamic proof is unsupported

- **WHEN** the platform or resolved backend has no supported dynamic containment probe
- **THEN** doctor reports not-supported/not-proven and never substitutes host writeability or backend presence for a pass

### Requirement: Unsafe full-access stage workers fail closed before launch

An automated stage worker whose inner harness is configured for full access
SHALL NOT launch when the resolved outer backend is `none` or otherwise
uncontained. Admission SHALL place the stage on an actionable infrastructure
hold, and doctor SHALL report the same unsafe resolution. This fail-closed rule
SHALL be based on the effective backend after fallback, not merely the requested
backend.

#### Scenario: Requested containment degrades to none

- **WHEN** a full-access stage requests bwrap but backend resolution falls back to `none`
- **THEN** stage admission refuses the launch as uncontained and doctor names both the requested and effective backend

#### Scenario: Full-access harness has effective outer containment

- **WHEN** a full-access stage resolves to a supported outer backend and the containment plan passes its required checks
- **THEN** admission may launch it and reports the outer backend as the effective security boundary

### Requirement: Linux bwrap smoke proves commit capability and containment together

The automated Linux/bwrap smoke SHALL create an isolated disposable repository
with a linked worktree, isolated Git identity, fake provider home, and a narrowly
selected outside sentinel. Through the production sandbox composer it SHALL
successfully write a probe file, run `git add` and `git commit`, verify that
`HEAD` tracks the commit, and prove a write to the outside sentinel is denied.
It SHALL clean up every probe path/process and SHALL NOT target a user's real
repository, broad home path, or credentials.

#### Scenario: A contained worker commits

- **WHEN** the bwrap smoke executes against its disposable linked worktree
- **THEN** the in-sandbox commit succeeds and host-side verification finds the probe file in the new `HEAD`

#### Scenario: The same worker attempts an outside write

- **WHEN** the smoke payload writes to its purpose-created sibling sentinel outside the allowed worktree/Git paths
- **THEN** the write is denied while the successful commit remains intact

#### Scenario: The smoke exits early

- **WHEN** setup, containment, Git, assertion, or test execution fails
- **THEN** scoped cleanup removes only the fixture-owned paths and terminates only fixture-owned probe processes

### Requirement: Commit containment and compiler-cache health remain independent

Stage-worker commit probes and sandbox compiler-cache probes MAY share backend
execution, timeout, redaction, and unsupported-state infrastructure, but SHALL
produce independent results and remediation.

#### Scenario: The worker can commit but sccache is unreachable

- **WHEN** commit containment passes and the optional contained compiler-cache probe fails
- **THEN** doctor reports the worker contained and the cache degraded, without converting either result into the other
