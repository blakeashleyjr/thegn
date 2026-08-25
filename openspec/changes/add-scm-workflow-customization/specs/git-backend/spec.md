# Git Backend

## ADDED Requirements

### Requirement: Colocated jujutsu repos are detected and not fought

thegn SHALL detect a colocated jujutsu repository (a `.jj` directory beside
`.git` at the workspace root) without spawning any `jj` process, and SHALL
degrade rather than interfere: detached HEAD in a colocated repo MUST NOT be
surfaced as an error state, staging and commit surfaces MUST show a notice
that jj ignores the git index, and background `auto_fetch` MUST skip
colocated repos unless `[git] auto_fetch_colocated = true`. Read operations
continue unchanged; mutating operations are warned about, not blocked.

#### Scenario: Colocated repo is detected and badged

- **WHEN** a workspace root contains `.jj/` beside `.git/`
- **THEN** the worktree rows carry a jj indicator and `thegn doctor` reports
  the colocation, with no `jj` subprocess spawned

#### Scenario: Detached HEAD is normal there

- **WHEN** a colocated repo's HEAD is detached (jj's normal state)
- **THEN** thegn renders the working-copy state without an error or warning
  styling reserved for broken repos

#### Scenario: auto_fetch stays out of jj's way

- **WHEN** `[git] auto_fetch = true` and a repo is colocated with default
  settings
- **THEN** the background fetch skips that repo, and setting
  `auto_fetch_colocated = true` restores it

### Requirement: Git workflow keys resolve per workspace

thegn SHALL support a `[workspace.<slug>.git]` overlay in the trusted user
configuration, resolved by a single `Config::repo_git(root)` accessor that
every repo-scoped `[git]` consumer uses (mirroring `repo_merge_queue`), so
signing, fetch, and diff-view policy can differ per repository. The untrusted
repo-root `.thegn.*` overlay MUST NOT be able to set `[git]` keys.

#### Scenario: One repo overrides its git policy

- **WHEN** the user config carries `[workspace.acme.git] structural_diff = "difft"`
- **THEN** the acme workspace resolves that value while other workspaces keep
  the global one

#### Scenario: Untrusted overlay cannot set git keys

- **WHEN** a repo-root `.thegn.toml` contains a `[git]` table
- **THEN** it is rejected as an unknown key by the repo-overlay schema and
  reported, never applied

### Requirement: Doctor reports source-control workflow posture

`thegn doctor` SHALL report the repo's workflow posture in one section: the
installed git version against the object-DB fold's `merge-tree --write-tree`
floor (git ≥ 2.38), jj colocation per workspace, whether the repo declares
custom `.gitattributes` merge drivers, and — only when `[merge_queue]
sign_commits` is enabled — whether a commit can be signed non-interactively
under the active identity. The posture checks MUST be cheap and local (no
network), and `--json` output MUST carry the same facts.

#### Scenario: Old git fails the fold floor

- **WHEN** the installed git predates `merge-tree --write-tree`
- **THEN** doctor flags that the merge queue's fold cannot work and names the
  minimum version

#### Scenario: Signing readiness probed only when relevant

- **WHEN** `sign_commits` is disabled
- **THEN** doctor performs no signing probe and prints the policy as off
