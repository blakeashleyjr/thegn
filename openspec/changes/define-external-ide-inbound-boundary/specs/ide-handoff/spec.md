# IDE Handoff — inbound boundary delta

## ADDED Requirements

### Requirement: Outbound editor handoff does not imply inbound IDE support

Public documentation and schemas SHALL distinguish outbound `editor.open` from
inbound IDE/OS integration. Until an inbound implementation change satisfies
the adoption gate, thegn MUST NOT claim a `thegn://` handler, OS association,
official IDE extension, reverse reveal RPC, or IDE-specific socket.

#### Scenario: Extension author reads the public contract

- **WHEN** an author looks for a supported way to make an IDE focus thegn
- **THEN** the contract says inbound integration is unsupported/undecided rather
  than inferring it from outbound `editor.open`

### Requirement: Any inbound adoption uses an explicit security contract

Before an inbound surface is advertised, its implementation change SHALL define
platform ownership, authentication and scopes, repo/worktree resolution,
canonical path confinement, parameter bounds, user-confirmation requirements,
expiry/replay behavior, errors, packaging/uninstall, schemas, and tests. It MUST
NOT execute link-provided commands or introduce a parallel authentication table.

#### Scenario: URL handler is proposed

- **WHEN** a future change proposes `thegn://open`
- **THEN** it cannot land until strict parsing, containment, platform
  registration, authorization, and rollback are all specified and tested
