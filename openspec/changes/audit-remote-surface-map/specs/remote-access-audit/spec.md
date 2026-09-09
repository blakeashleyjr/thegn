# Remote access audit

## ADDED Requirements

### Requirement: The remote surface map is versioned and traceable

The repository SHALL contain a versioned remote-access audit identifying its
date, audited commit, scope, methodology, accountable owner, and next refresh
trigger. Stable row/finding IDs and direct evidence SHALL allow THE-98, THE-41,
and bounded child issues to link to exact conclusions.

#### Scenario: Code changes after an audit

- **WHEN** a named refresh trigger occurs
- **THEN** the owner updates the audit commit/date, affected evidence and issue
  classifications rather than leaving the old map implicitly current

### Requirement: Every remote architecture layer is inventoried

The audit SHALL inventory interactive transport, non-interactive control,
data/file projection, Git/forge reads and writes, provider lifecycle/egress/
checkpoints/files, reverse tunnels/Iroh, merge queue, control API, MCP/plugin
projection, credentials, config/trust, UI status, recording, and diagnostics.
Every surface row MUST record owner/module, stability, authentication,
confidentiality, extension seam, platform/provider support, failure behavior,
tests, documentation, diagnostics/status, and gaps.

#### Scenario: A surface has no test or encryption contract

- **WHEN** evidence for a required field is absent
- **THEN** the row records an explicit gap/unknown and links accepted follow-up
  work rather than assuming coverage

### Requirement: The prior ten findings are completely reconciled

The audit SHALL recover or reconstruct all ten findings referenced by the prior
Linear comment and classify each as `resolved`, `mitigated`, `accepted_risk`,
`duplicate`, or `outstanding`. Each classification MUST include concrete code/
test/doc evidence, rationale, owner, and an issue link for outstanding accepted
work.

#### Scenario: Original wording cannot be recovered

- **WHEN** a historical finding is unavailable
- **THEN** the audit explicitly marks it reconstructed, documents the evidence
  and method, and still accounts for one of the ten slots

### Requirement: Reference-product capabilities receive explicit decisions

The audit SHALL classify RDP/VNC/Telnet, SFTP/file management, tunnels,
host/container management, fleets, IAM/RBAC, alerts, session sharing,
recording, serial, tailnet, Proxmox, desktop sync, CLI, localization, cloud
SDKs, and object storage as `Shipped`, `Partial`, `Candidate`, or `Non-goal`.
Every Candidate and Non-goal MUST include product rationale and revisit
criteria; reference parity MUST NOT be inferred.

#### Scenario: A reference feature is attractive but unaccepted

- **WHEN** no Thegn use case or operating-cost decision accepts the feature
- **THEN** it is Candidate or Non-goal with rationale, not an implementation
  commitment

### Requirement: Accepted gaps map to bounded owned issues

The audit SHALL link existing issues wherever work is already tracked and SHALL
create a new issue only for an accepted, non-duplicate gap. Every outstanding
accepted issue MUST include owner, priority, dependencies, acceptance criteria,
and parent/overlap links.

#### Scenario: An audit row overlaps existing work

- **WHEN** the gap is already owned by another issue or OpenSpec change
- **THEN** the row links/classifies that work and no duplicate issue is created
