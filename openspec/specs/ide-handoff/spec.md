# ide-handoff Specification

## Purpose

TBD - created by archiving change add-external-ide-handoff. Update Purpose after archive.

## Requirements

### Requirement: Public outbound handoff is editor.open

The capability catalog SHALL expose `editor.open` as the canonical outbound
handoff operation on its supported public transports. The request SHALL name a
registered worktree and a project or contained file target with optional line
and column. Successful acceptance means a bounded-lifetime intent was enqueued;
unknown fields, unknown worktrees, expired requests, and escaping paths SHALL
fail explicitly.

#### Scenario: Remote client opens a contained file

- **WHEN** an authorized client calls `editor.open` for a file beneath a known
  worktree
- **THEN** the request is strictly decoded, enqueued, and launched through the
  same editor seam as native UI actions

#### Scenario: Escaping file is rejected

- **WHEN** a request resolves outside the selected worktree
- **THEN** it fails before any editor process is launched

### Requirement: Handoff execution does not block the UI loop

Target resolution and process launch SHALL execute outside the compositor loop,
with completion or failure delivered through the normal async/waker path.

#### Scenario: Editor startup is slow

- **WHEN** a configured editor takes time to resolve or start
- **THEN** the compositor remains responsive and receives only the resulting
  completion state

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
