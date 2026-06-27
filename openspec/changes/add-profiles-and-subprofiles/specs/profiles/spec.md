# Profiles

## ADDED Requirements

### Requirement: A profile is a firewalled separate process

A profile SHALL run as its own OS process rooted at a profile scope resolved from `--profile`/`SUPERZEJ_PROFILE` (default `default`) and set into the process environment before any thread or DB opens, firewalling state/DB, config, credentials + git identity, and sandbox/network policy; a per-profile advisory `flock` MUST prevent a second process for the same profile without busy-polling.

#### Scenario: Profile reroots its storage

- **WHEN** superzej starts with `SUPERZEJ_PROFILE=work`
- **THEN** its DB, logs, activity, and sockets resolve under `profiles/work/`
  while worktrees stay at their existing absolute paths

#### Scenario: Singleton per profile

- **WHEN** a second process starts for an already-running profile
- **THEN** the non-blocking `flock` fails and the second process does not open the
  profile's DB, with no CPU spin

### Requirement: Shared base config plus per-profile overlay

Configuration SHALL layer defaults → shared base config (loaded from the real `XDG_CONFIG_HOME`) → per-profile overlay → per-subprofile overlay → the existing per-workspace/env/`--set` layers, with more specific layers winning.

#### Scenario: Profile overlay wins over base

- **WHEN** the shared base sets a value and the active profile overrides it
- **THEN** the profile's value takes effect while unset keys fall through to the
  shared base

### Requirement: Credentials are firewalled per profile

Pane environments SHALL be assembled clear-then-allowlist (a curated base plus profile credentials) rather than inheriting the launching shell's environment, and profile-scoped credential variables (`GIT_CONFIG_GLOBAL`, `GH_CONFIG_DIR`, `GIT_SSH_COMMAND` with `IdentitiesOnly=yes`, `GNUPGHOME`) MUST point at the profile's identity.

#### Scenario: Launching-shell tokens do not leak

- **WHEN** a shell pane is spawned under a profile
- **THEN** tokens/keys the launching shell exported are absent and `git config
user.email` resolves to the profile's identity

### Requirement: A subprofile scopes one subsystem in-process

A subprofile switch SHALL re-scope a single subsystem (its storage handle, credential scope, and pane set) without touching other subsystems, and MUST NOT introduce any polling.

#### Scenario: Comms switch leaves workspace untouched

- **WHEN** the comms subsystem switches from `work` to `personal`
- **THEN** comms panes and DB handle rebind while the workspace subsystem's panes
  and state are unaffected and no idle wakeups are added
