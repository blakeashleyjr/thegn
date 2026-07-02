# Sandbox

## ADDED Requirements

### Requirement: A declarative policy governs brokered file and shell operations

The sandbox broker SHALL govern the file (read/write/delete) and shell operations
it services with an ordered `[policy]` rule set evaluated by a pure decision
function, yielding allow, deny, or ask. A rule MATCHES when every present
selector (operation kind, path glob evaluated relative to the worktree root via
`under:`/`outside:` helpers, command regex) matches; the first matching rule wins,
and a rule marked `stop` short-circuits. When no rule matches, the configured
default for that operation kind applies. An empty policy MUST reproduce the prior
behavior.

#### Scenario: Read under the worktree is allowed silently

- **WHEN** a brokered read targets a path under the worktree root and the default
  read action is allow
- **THEN** the operation is serviced without a prompt

#### Scenario: Write outside the worktree is denied

- **WHEN** a rule denies writes to `outside:.` and a brokered write targets a path
  outside the worktree root
- **THEN** the operation is refused with the rule's reason

#### Scenario: First matching rule wins

- **WHEN** two rules match an operation
- **THEN** the earlier rule's action is applied

### Requirement: Hard denies cannot be overridden by user allow rules

The policy engine SHALL enforce built-in hard denies for writes and deletes to
sensitive paths (the shared `.git/config`, `~/.ssh`, `~/.gnupg`, and configured
secret paths), and a user `allow` rule MUST NOT override a hard deny.

#### Scenario: Allow rule cannot open a secret path

- **WHEN** a user rule allows writes broadly and a brokered write targets `~/.ssh`
- **THEN** the operation is still denied by the hard-deny rule

### Requirement: Ask decisions use the existing permission flow and can persist

An `ask` decision SHALL raise the existing sandbox/ACP permission overlay rather
than auto-allowing, and an `allow_always` or `reject_always` choice MUST persist
as an appended policy rule so the next identical operation is decided without a
prompt. Policy MUST narrow, never widen, the container/bouncer isolation, which
remains the hard boundary.

#### Scenario: Ask prompts the user

- **WHEN** an operation resolves to ask
- **THEN** the permission overlay is shown and the operation proceeds only on
  approval

#### Scenario: Always-choice is remembered

- **WHEN** the user answers an ask with allow-always
- **THEN** a matching allow rule is persisted and the next identical operation is
  allowed without a prompt
