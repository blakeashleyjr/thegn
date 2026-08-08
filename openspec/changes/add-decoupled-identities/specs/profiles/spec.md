# Profiles

## MODIFIED Requirements

### Requirement: Credentials are firewalled per profile

Pane environments SHALL be assembled clear-then-allowlist (a curated base plus
profile credentials) rather than inheriting the launching shell's environment, and
profile-scoped credential variables (`GIT_CONFIG_GLOBAL`, `GH_CONFIG_DIR`,
`GIT_SSH_COMMAND` with `IdentitiesOnly=yes`, `GNUPGHOME`) MUST point at the
profile's identity. When the profile references a named identity
(`[profiles.<p>].identity`), each credential variable SHALL resolve **per tool**
from that identity, falling back independently to the profile's own
`<profile_root>/<tool>` path for any tool the identity leaves unset; a profile
that references no identity SHALL resolve exactly the profile-root paths as before.

#### Scenario: Launching-shell tokens do not leak

- **WHEN** a shell pane is spawned under a profile
- **THEN** tokens/keys the launching shell exported are absent and `git config
user.email` resolves to the profile's identity

#### Scenario: Per-tool identity resolution with fallback

- **WHEN** a profile references an identity that sets only git and SSH
- **THEN** `GIT_CONFIG_GLOBAL` and `GIT_SSH_COMMAND` resolve from that identity
  while `GH_CONFIG_DIR` and `GNUPGHOME` fall back to the profile-root paths, and
  the sandbox credential mounts point at the resolved directories
