# env-bundles Specification

## Purpose

TBD - created by archiving change add-env-bundles. Update Purpose after archive.

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
