# env-bundles Specification

## Purpose

Named env bundles bound at any scope and resolved through one pane-spawn compose seam: config-dir redirection, opt-in allowlisted .env, off-loop secret resolvers, and dotfile / synthetic-HOME tiers.

## Requirements

### Requirement: Named bundles bind at any scope and compose

A bundle SHALL be a named declarative unit (`[bundle.<name>]`) of env vars, account selections, config-dir redirections, optional dotfiles, and an optional synthetic HOME, bindable per global/workspace/worktree, and the effective environment MUST be the per-key-override merge of (curated base ◁ extends chain ◁ global ◁ workspace ◁ worktree).

#### Scenario: Worktree bundle refines workspace bundle

- **WHEN** a worktree binds a bundle and its workspace binds another
- **THEN** the worktree bundle's keys override while unset keys fall through to
  the workspace bundle rather than discarding it

### Requirement: A single compose seam resolves env for every pane

Pane environment SHALL be resolved by one core `compose()` seam returning env overrides, blocked keys, and mounts, and it MUST be applied to every pane spawn — shells as well as agents — so a shell in a bound worktree sees that bundle's identity rather than the launching shell's.

#### Scenario: Shell pane gets the bundle identity

- **WHEN** a plain shell pane is spawned in a worktree bound to the `work` bundle
- **THEN** it sees the work git identity / account, not the launching shell's
  credentials

### Requirement: Opt-in .env is allowlisted, low-precedence, and filtered

A worktree `.env` SHALL load only when explicitly opted in and allowlisted by content hash, MUST load at low precedence (never overriding bundle-set values), and MUST drop credential-like keys (`*_TOKEN`/`*_KEY`/`*_SECRET`/`*_PASSWORD`) by default.

#### Scenario: .env cannot inject a secret or override creds

- **WHEN** an allowlisted `.env` defines `FOO` and `SECRET_KEY`
- **THEN** `FOO` fills only an unset gap and `SECRET_KEY` is filtered out

### Requirement: Secrets resolve at launch and are never persisted

Bundle values referencing a secret resolver SHALL be resolved at launch time off the event loop and MUST NOT be persisted to config, DB, or logs; a resolver failure MUST degrade (warn + skip the key) without blocking the spawn.

#### Scenario: Resolved secret is not persisted

- **WHEN** a bundle value resolves via a secret resolver
- **THEN** the value reaches the child environment but appears in no persisted
  store or log

### Requirement: A zone-owned bundle is a credential sub-vault

A bundle MAY declare an owning zone. A zone-owned bundle SHALL be composable only
by a worktree that belongs to that zone; a foreign or unzoned worktree that would
compose it (whether bound directly, at the workspace or global scope, or reached
via another bundle's `extends`) has it skipped and the denial recorded, and the
launch continues without it. A global (unzoned) bundle remains composable
everywhere.

#### Scenario: A foreign worktree is denied a zone-owned bundle

- **WHEN** a worktree in zone `clientB` composes a bundle owned by zone `clientA`
- **THEN** the bundle is skipped, the denial is recorded, and its env is not
  applied

#### Scenario: A member composes its own zone's bundle

- **WHEN** a worktree in zone `clientA` composes a bundle owned by `clientA`
- **THEN** the bundle's env is applied

#### Scenario: extends reachability is covered

- **WHEN** a visible bundle `extends` a foreign zone-owned bundle
- **THEN** the visible bundle folds but the foreign parent is denied

#### Scenario: A global bundle stays usable inside a zone

- **WHEN** a worktree in a zone composes a bundle with no owning zone
- **THEN** the bundle's env is applied
