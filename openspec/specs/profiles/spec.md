# profiles Specification

## Purpose

Firewalled per-profile isolation: each profile is a separate rerooted process (state, config, credentials, sandbox) guarded by a flock singleton, with in-process subprofile rescoping of a single subsystem.

## Requirements

### Requirement: A profile is a firewalled separate process

A profile SHALL run as its own OS process rooted at a profile scope resolved from `--profile`/`THEGN_PROFILE` (default `default`) and set into the process environment before any thread or DB opens, firewalling state/DB, config, credentials + git identity, and sandbox/network policy; a per-profile advisory `flock` MUST prevent a second process for the same profile without busy-polling.

#### Scenario: Profile reroots its storage

- **WHEN** thegn starts with `THEGN_PROFILE=work`
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

### Requirement: A subprofile scopes one subsystem in-process

A subprofile switch SHALL re-scope a single subsystem (its storage handle, credential scope, and pane set) without touching other subsystems, and MUST NOT introduce any polling.

#### Scenario: Comms switch leaves workspace untouched

- **WHEN** the comms subsystem switches from `work` to `personal`
- **THEN** comms panes and DB handle rebind while the workspace subsystem's panes
  and state are unaffected and no idle wakeups are added

### Requirement: A session move transfers the selected worktree state cold

Thegn SHALL provide the admin-only CLI operation
thegn session move <worktree> --to-profile <name> [--kill] [--dry-run] [--json]
(sessions.migrate, Verb::MigrateSession, CLI surface only). The selector MUST
be the exact stored worktree path in the active source session. It SHALL select
every tab_groups row for that session and path, including multiple groups, and
all of their group_tabs rows. The target profile MUST already exist and the
operation MUST NOT reroot the process, load target config, or copy credentials,
identities, tokens, accounts, pairings, secrets, layouts, global caches, queue
state, or other profile-global data.

The transferable set SHALL additionally include the source worktree
registration when present, selected sidebar ui_state rows under the exact
collapse:<group>, pin:<group>, and pin_ordinal:<group> segments, every
agent_dispatches row for the path, its agent_dispatch_notes, and only the
active session's session_state.pin_state. It MUST preserve opaque pane
commands, cwd data, scrollback, dispatch reports, artifact/chunk paths, and
notes without printing their contents. It MUST exclude session_attention,
whole-session active-tab state, and all unrelated rows. Worktree paths, artifact
paths, branches, group ordinals, and tab ordinals remain unchanged.

The move is cold by definition. Live source daemon sessions are the union of
sessions owning the exact worktree, daemon ids referenced by selected pane
state, and dispatch session ids. Without --kill, any confirmed live id MUST be
listed and block the move. With --kill, each live id MUST be stopped through
the source daemon control client and a subsequent listing MUST confirm that no
survivor remains. A registered but unreachable source daemon MUST fail closed
when referenced sessions cannot be disproved. Imported pane_sessions and
dispatch session_id values MUST be cleared; target resurrection creates fresh
target-profile daemon sessions.

#### Scenario: A cold move carries all selected state

- **WHEN** a worktree has no live source daemon sessions and is moved to an
  existing target profile
- **THEN** all selected groups/tabs, worktree registration, sidebar state,
  dispatches/notes, and the active session's running-pin state are available in
  the target, while target-owned worktree metadata and active-tab focus are
  preserved

#### Scenario: Live sessions block without explicit kill

- **WHEN** a selected worktree has live daemon sessions and --kill is absent
- **THEN** the move reports the exact live ids and both stores are unchanged

#### Scenario: Kill is explicit and source-scoped

- **WHEN** the user repeats the move with --kill
- **THEN** the source control seam kills and re-lists the sessions before any
  target write, and imported rows contain no source daemon ids

#### Scenario: A source compositor cannot race cleanup

- **WHEN** another interactive instance owns the source profile
- **THEN** the move refuses before opening either migration store

#### Scenario: Credentials remain within each process profile

- **WHEN** state is imported and later resurrected by the target profile
- **THEN** the panes use only the target profile's config, identity, and
  clear-then-allowlist environment; no source credential-bearing data crosses

### Requirement: A session move is target-first, resumable, and auditable

The move SHALL preflight target group keys (session_name, group_name),
selected sidebar keys, and running-pin state before writing. A differing target
collision MUST abort before either store is changed. An existing target
worktree registration is target-owned and is not overwritten or treated as a
conflict. Each store mutation SHALL be one transaction: the target commit MUST
precede exact source cleanup. The target MUST be read back and matched against
a stable fingerprint over sanitized transferable rows before source cleanup.
If target commit succeeded but confirmation or cleanup failed, the command MUST
return its retryable outcome and report the partial state. Retrying an identical
committed import SHALL adopt it, avoid duplicate rows, and complete pending
source deletion; a different prior import MUST abort.

--dry-run SHALL use read-only database access with no create, migration, prune,
journal-mode change, WAL, journal, or schema-file side effect. It MUST report
the source/target profiles, exact worktree, selected groups, row counts, live
and would-be-killed ids, conflict result, and an opaque-payload warning stating
that pane commands, scrollback, dispatch reports, and notes are carried
unchanged and are not included in the audit. This warning and all audit fields
MUST be present in human and JSON output, including when no live sessions
exist. Neither output mode may serialize opaque row contents or credentials.

After confirmed cleanup, target daemon notification is best effort and is
reported as sent, unavailable, or failed; notification failure MUST NOT undo a
confirmed move. Human and JSON output SHALL identify target_committed,
target_confirmed, source_deleted, resumed, per-table counts, and notification
status.

#### Scenario: A collision aborts before writing

- **WHEN** a target group, sidebar key, or running-pin payload has a different
  value from the selected source row
- **THEN** the command names the conflict and neither store changes

#### Scenario: An interrupted move favors duplication over loss

- **WHEN** the process stops after target commit and before source cleanup
- **THEN** the target remains readable, the source remains intact, and retry
  confirms the same fingerprint without re-importing it before deleting source
  rows

#### Scenario: Dry-run is strictly read-only and complete

- **WHEN** the user runs --dry-run against an absent or stale target DB
- **THEN** the command does not create or change database sidecars or schema,
  reports the preflight result or a read-only schema error, and includes the
  opaque-payload warning without emitting payload text

#### Scenario: A running target receives a best-effort notice

- **WHEN** the target daemon is discoverable through the target DB registry
- **THEN** it receives a non-secret session-migrated notification; an
  unreachable or absent daemon is reported as a warning after the move

### Requirement: Profiles have a stable, user-editable switcher order

The profile switcher SHALL render profiles in a user-controlled order persisted in
shared, never-rerooted config (read from the real `XDG_CONFIG_HOME`, not any
per-profile state root), and the user MUST be able to reorder the highlighted
profile from within the switcher; profiles absent from the stored order SHALL be
appended in a stable, deterministic order rather than reordering the known ones.

#### Scenario: Switcher renders the stored order

- **WHEN** the profile switcher opens and a shared profile order is present
- **THEN** profiles appear in that order, with the active profile still marked,
  and any profile not in the stored order is appended deterministically after it

#### Scenario: Reordering persists across profiles and restarts

- **WHEN** the user moves the highlighted profile up or down in the switcher
- **THEN** the entire new order is written to shared config and the next switcher
  open — in this or any other profile's process — reflects it

#### Scenario: Order state is not per-profile

- **WHEN** the order is read or written
- **THEN** it resolves under the real `XDG_CONFIG_HOME` (never a rerooted
  per-profile state root), so every profile's process observes one shared order
