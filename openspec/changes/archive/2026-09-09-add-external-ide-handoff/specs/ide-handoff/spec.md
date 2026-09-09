# IDE Handoff

## ADDED Requirements

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
