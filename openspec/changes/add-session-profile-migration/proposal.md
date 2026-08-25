# Move sessions to a different profile

Linear: THE-55

## Why

Profiles are whole-process firewalls: separate state/DB, config overlay,
credentials + git identity, sandbox policy, and — because the daemon socket
lives under the profile state root — a **separate pane daemon per profile**.
That firewall is the point, and it is also why sessions get stranded: a user
who started a client's worktree session under `default` and later created a
`client-acme` profile has no way to move the session where its credentials
and config belong. The research seed (agent-of-empires #1212) lands on the
same shape and the same rules: move the session's _metadata_; the profile's
config, environment, sandbox defaults and tokens explicitly do **not** move —
the session picks up the target profile's on next start.

thegn's version has one honest constraint the seed's tmux design dodges: a
**live** daemon session's process was spawned with the source profile's
credential environment (clear-then-allowlist composition). No IPC can change
a running process's environment — "moving" it live would smuggle profile A's
credentials into profile B's window and break the firewall's core promise.
So the move is **cold by definition**: the persisted resurrection state
moves; live processes are stopped (with consent) and respawn under the
target's identity.

The natural unit is the **worktree group** — the resurrection unit the DB
already models (`tab_groups` row + its `group_tabs`: tab titles, pane trees,
cwds, scrollback snapshots). Worktrees themselves live at absolute paths
shared across profiles (the profiles spec: reroot moves state, not
worktrees), so moving a group re-homes _which profile owns the session
context_, not the files.

## What Changes

- **CLI:** `thegn session move <worktree-or-group> --to-profile <name>
[--kill] [--dry-run]`, runnable under either profile (`--profile` already
  reroots every subcommand). Moves the group's `tab_groups`/`group_tabs`
  rows (and the worktree's registration row if the target lacks it) from the
  source profile's DB to the target's.
- **Safety rules (the spec's core):**
  - Live daemon sessions referenced by the group (`group_tabs.pane_sessions`)
    block the move; `--kill` stops them first via the source profile's
    daemon. Never a silent kill.
  - **No credentials cross.** Nothing environment- or identity-shaped is
    copied (none is stored in these rows today; the requirement pins that it
    stays true). `pane_sessions` ids are cleared (they name the _source_
    daemon's sessions); panes respawn under the target profile's composed
    environment on next start.
  - Collision on the target's `(session_name, group name)` key bails before
    any write; the move is transactional per side (source delete only after
    target insert commits) and idempotently resumable.
  - Scrollback snapshots move with the group (they are the resurrection
    value) — the design's Security section calls out that this is terminal
    output crossing the firewall by explicit user action.
- **Visibility:** the moved group appears in the target profile at its next
  launch/resurrect (matching the seed's "picks them up on next start");
  best-effort, if the target profile's daemon is reachable, a notification is
  pushed through its existing `notify.push` door so a running target learns
  about it. The source side reports what moved and what was killed.
- **Catalog:** one new row — `sessions.migrate` (`Verb::MigrateSession`),
  **Admin** scope, **CLI surface only**. Admin caps never reach MCP/plugin
  (existing catalog rule); the operation spans two profile stores and is
  deliberately not remotable in v1.

## Impact

- **Roadmap:** group H (profiles) gains its cross-profile mobility item;
  touches I-115/118 (session naming/list) only as consumers. tasks.md wiring
  happens in the audit phase.
- **Specs:** `profiles` — ADDED cross-profile move requirement (this folder).
  No `state-db` schema change: the move uses existing tables; no new columns.
- **In-flight changes reconciled:** **add-profile-reordering** (profile
  list/switcher UX — no overlap with move mechanics),
  **add-runtime-session-split** (once the daemon owns the session model, the
  export step reads the model from the source daemon instead of the DB rows;
  the cold-move semantics and safety rules are unchanged — noted so neither
  change blocks the other), **make-daemon-default** (live sessions being the
  default is exactly why `--kill` consent exists),
  **add-decoupled-identities** (identity stays profile-scoped; the move spec
  leans on that boundary, does not alter it).
- **Help/config:** CLI verb documented in `docs/help/cli.md` +
  `docs/help/daemon-and-sessions.md`; no new config keys.

## Non-goals

- **Live migration.** Moving a running process between profile daemons
  (fd-passing the PTY master) is technically possible on unix and rejected
  on principle: the process keeps the source profile's credential
  environment, which is precisely what the firewall exists to prevent.
- **Moving worktrees on disk.** Worktrees stay at their absolute paths;
  git remains the source of truth.
- **Copying or merging profile config/credentials/identity** — explicitly
  never.
- **A TUI drag gesture for cross-profile moves.** Profiles are separate
  processes with separate windows; v1 is CLI-first (a palette action in the
  source profile can invoke the same verb later).
- **Batch/bulk move UX** beyond accepting multiple selectors on the CLI.
