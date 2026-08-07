# Identities

## ADDED Requirements

### Requirement: A named identity binds credential tools independently

An identity SHALL be a named set of optional, per-tool credential bindings — git
config, git SSH key, GitHub/forge config, GPG home — plus per-provider agent
account selection, defined once and independent of any profile; each tool binding
SHALL be optional so an identity may set some tools and leave others unset.

#### Scenario: A partial identity leaves unset tools unbound

- **WHEN** an identity sets only `git.config` and `gh.config`
- **THEN** resolving it yields those two bindings and leaves GPG and SSH unset,
  to be filled by a less specific layer or the profile fallback

### Requirement: Profiles and bundles reference an identity by name, per tool

A profile (`[profiles.<p>].identity`) and an env-bundle (`[bundle.<name>].identity`)
SHALL be able to reference a named identity, resolved **per tool**, so different
tools may come from different identities (mix-and-match); a tool set by a more
specific scope SHALL override that tool while tools it leaves unset fall through
to a less specific scope and finally to the profile's own paths.

#### Scenario: Mix git and gh from different identities

- **WHEN** a profile references identity `washu` (git + ssh) and an in-scope
  bundle references identity `personal` (gh only)
- **THEN** the pane's git/ssh come from `washu`, its GitHub config comes from
  `personal`, and GPG (set by neither) falls through to the profile fallback

#### Scenario: An identity is reused across profiles

- **WHEN** two profiles both reference identity `washu`
- **THEN** both resolve the same per-tool bindings without duplicating the
  configuration

### Requirement: Identity resolution preserves the credential firewall

Resolving an identity SHALL preserve the profile firewall invariants: forge tokens
(`GH_TOKEN`/`GITHUB_TOKEN`) remain unset, `GIT_SSH_COMMAND` forces
`IdentitiesOnly=yes`, sandbox credential mounts point at the **resolved** per-tool
directories, and a profile or bundle that references no identity SHALL behave
exactly as before (identities are additive, zero-migration).

#### Scenario: No identity referenced ⇒ unchanged behavior

- **WHEN** no `[[identities]]` exist and nothing sets `identity =`
- **THEN** every profile and bundle resolves the same credential paths as before
  the feature, with no migration

#### Scenario: Tokens never leak through an identity

- **WHEN** a pane spawns under a profile or bundle that resolves an identity
- **THEN** the launching shell's forge tokens are absent and `git config
user.email` resolves to the identity's git config

### Requirement: The active identity is switchable at any scope without restart

The user SHALL be able to switch the active identity from a switcher, binding it at
worktree, workspace, or global scope; the binding SHALL take effect for
subsequently spawned panes without restarting the process and MUST NOT introduce
any polling.

#### Scenario: Switching identity rebinds new panes

- **WHEN** the user selects a different identity in the switcher at worktree scope
- **THEN** panes spawned afterward in that worktree resolve the new identity's
  per-tool bindings, existing panes are unaffected, and no idle wakeups are added
