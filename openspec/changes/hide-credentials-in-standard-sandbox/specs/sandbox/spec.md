# Sandbox

## ADDED Requirements

### Requirement: Standard hides credentials and sibling repositories

A local `standard` sandbox with worktree file access SHALL hide, from its
read-only `$HOME`, the well-known credential locations that exist (including
`~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.kube`, `~/.docker/config.json`,
`~/.netrc`, `~/.git-credentials`, `~/.config/gh`, `~/.secrets`, gpg private
keys, keyrings and browser profiles), every `~/.ssh` entry other than
`known_hosts`, `known_hosts.old`, `config`, `authorized_keys` and `*.pub`, and
every git repository that is not dot-named and sits in the parent directory of
the worktree or of its main checkout. The worktree, its main checkout and its
git common dir SHALL stay visible. bwrap and local podman/docker SHALL mask a
hidden directory with an empty tmpfs and a hidden file with a read-only
`/dev/null` bind wherever a bind exposes it; systemd SHALL make it
inaccessible unless that would hide the worktree or a mount. A hidden path at
or above an explicit sandbox mount destination SHALL NOT be masked. `standard`
SHALL NOT bind the link targets of `~/.ssh` identity keys. The `sealed`
profiles are unchanged.

#### Scenario: Credentials are unreadable in a standard bwrap pane

- **WHEN** a standard bwrap pane runs with `~/.aws/credentials`,
  `~/.netrc` and `~/.ssh/id_ed25519` present on the host
- **THEN** the pane sees an empty `~/.aws`, an empty `~/.netrc` and an empty
  `~/.ssh/id_ed25519`, while `~/.ssh/known_hosts` and `~/.ssh/config` stay
  readable

#### Scenario: Sibling repositories are hidden

- **WHEN** a standard pane opens a linked worktree whose main checkout sits
  beside other repositories
- **THEN** those repositories and the other worktrees beside this one appear
  empty, while this worktree and the main checkout's git dir stay usable

#### Scenario: An explicit mount wins

- **WHEN** a bundle identity mounts `~/.config/gh` into a standard pane
- **THEN** that directory is not masked

### Requirement: Standard keeps agent settings read-only

A non-sealed sandbox SHALL NOT carve a coding agent's config dir
(`~/.claude`, `~/.codex`, or the env-selected `CLAUDE_CONFIG_DIR` /
`CODEX_HOME`) read-write as a whole. It SHALL carve read-write only the
existing runtime-state entries the agent writes (for Claude Code: `projects`,
`todos`, `sessions`, `session-env`, `shell-snapshots`, `statsig`, `ide`,
`debug`, `file-history`, `plans`, `logs`, `history.jsonl`; for Codex:
`sessions`, `archived_sessions`, `log`, `shell_snapshots`, `history.jsonl`)
and the provider's login file. Settings, hooks, plugins, skills and installed
binaries in the dir SHALL stay read-only.

#### Scenario: A pane cannot rewrite agent settings

- **WHEN** a process in a standard pane writes `~/.claude/settings.json`
- **THEN** the write fails, while a write under `~/.claude/todos` succeeds

### Requirement: The profile explanation states what standard protects

The first-run sandbox step and the new-worktree dialog SHALL state that
`standard` hides credential files and sibling repositories and keeps agent
settings read-only, and that it does not stop a hostile agent because the rest
of home is readable and the network is open.

#### Scenario: The dialog explains standard

- **WHEN** the user selects `standard` in the new-worktree dialog or the
  first-run sandbox step
- **THEN** the panel names what is hidden and says it is not a boundary
  against a hostile agent
