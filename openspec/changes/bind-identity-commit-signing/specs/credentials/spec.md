# Credentials

## ADDED Requirements

### Requirement: Commit signing binds to the resolved identity with worktree override

A named identity SHALL optionally carry signing configuration (format
`openpgp` or `ssh`, plus a key id or path) that resolves alongside its other
tool bindings into the pane and git-operation environment (`gpg.format`,
`user.signingKey`), following the existing identity scope chain so a
worktree-bound identity overrides workspace and global bindings. The accepted
enable/default policy SHALL make the bound-identity scenario below true without
per-commit flags. The per-operation signing controls (commit overlay cycle,
rewrite signing override) SHALL continue to layer above the identity default
unchanged.

Private key material MUST NOT be copied into environment, argv, logs, config,
or the state DB. A host-only key path MUST NOT be silently passed to a remote
pane where it cannot resolve.

#### Scenario: Two worktrees sign with different keys

- **WHEN** worktree A is bound to identity `release` (ssh-format signing key)
  and worktree B has no binding
- **THEN** commits in A are ssh-signed with `release`'s key while commits in B
  follow the repo/global git config, with no per-commit flags needed

#### Scenario: Per-operation no-sign overrides the identity default

- **WHEN** a signing identity is active and the commit overlay selects no-sign
  or a background rewrite runs with `[git] override_gpg = true`
- **THEN** that operation creates no signature without changing the identity
  binding or the default for later operations

#### Scenario: A remote pane cannot receive a meaningless host key path

- **WHEN** an identity selects a signing key path that is unavailable in the
  resolved remote-provider filesystem
- **THEN** launch either provisions the material through an explicit secure
  custody path or fails with an actionable error before starting the pane
