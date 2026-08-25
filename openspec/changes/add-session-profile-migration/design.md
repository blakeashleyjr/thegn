# Design — cross-profile session move

## The operation is a store-to-store transfer, not IPC

Both profile stores belong to the same OS user; the firewall between profiles
is hygiene enforced by process rerooting, not a privilege boundary. The move
therefore runs as one CLI process that opens **two** SQLite stores directly:

1. **Resolve roots purely.** `profile::reroot` reroots the _process_ for one
   profile; the mover instead uses the pure scope-resolution helpers to
   compute both profiles' state roots (source = the active profile, target =
   `--to-profile`), without rerooting to the target. The target profile must
   exist (its root present) — no implicit profile creation.
2. **Preflight.** Read the group (`tab_groups` + `group_tabs`) from the
   source DB; resolve live daemon sessions from `pane_sessions` by asking the
   **source profile's daemon** (its socket, discovered exactly as
   `thegn session list` does); check the **target DB** for a
   `(session_name, name)` collision. `--dry-run` stops here and prints the
   plan: rows to move, sessions that would need killing, collision verdict.
3. **Stop live sessions** (only with `--kill`; otherwise refuse listing
   them). Kills go through the source daemon's existing `sessions.kill` — the
   mover never signals pids itself.
4. **Transfer.** Insert into the target DB in one transaction: the
   `tab_groups` row, its `group_tabs` rows with `pane_sessions` set to NULL
   (source-daemon ids are meaningless in the target daemon), and the
   worktree registration row if the target lacks one. Commit. Then delete the
   source rows in one transaction. Order makes a crash duplicate-visible
   (both sides briefly) rather than data-losing; re-running detects the
   already-committed target and finishes the source delete (idempotent
   resume). A duplicate group in a profile's own sidebar is self-evident and
   harmless; a vanished session is not.
5. **Report + notify.** Print what moved/killed. Best-effort: if the target
   daemon answers, push a note through its `notify.push` door ("group X moved
   here from profile Y — appears at next launch"). Never an error if the
   target daemon is down.

The running **source** UI, if any, self-reconciles the way it already does
for externally-removed state at next resurrect; v1 additionally sends the
source daemon's event feed a best-effort note so an attached UI can drop the
group without restart (deferred if the plumbing isn't there — the spec only
requires next-launch correctness on both sides).

## Concurrency with live profile processes

SQLite WAL allows a second process to write while a profile's UI runs; the
flock singleton guards the interactive process slot, not the DB. Writing the
target DB under a live target UI is safe (the UI reads it at
resurrect/persist boundaries) but the group appears only at next launch —
that asymmetry is stated, not hidden. Writing the **source** DB while its UI
runs risks the UI's debounced persist re-inserting the group after our
delete; therefore the mover refuses to move a group that is _open in a
running source compositor_ unless its panes are killed (`--kill` covers the
daemon sessions; an open-but-daemonless group in a live UI is detected via
the source flock + a warning to close it first). Cold source (no running
profile process) is the clean path and the primary use case.

## Why not a daemon-to-daemon protocol verb

A `sessions.migrate` RPC served by one daemon would need that daemon to reach
_another profile's_ socket and DB — exactly the cross-profile reach the
firewall exists to deny to long-running processes. Keeping the mover in a
short-lived CLI the _user_ invokes, with Admin scope in the catalog and CLI
as its only surface, keeps profile-crossing a deliberate human act. (The
catalog row exists so the operation is governed and enumerable like every
door; Admin caps are already barred from MCP/plugin by catalog test.)

## Alternatives considered

- **Live migration via SCM_RIGHTS fd-passing** (send the PTY master to the
  target daemon): mechanically feasible on unix, rejected — the child keeps
  the source profile's env/credentials, silently violating the firewall;
  also unportable (Windows named-pipe daemon).
- **Export/import as two user-run commands** (`session export` → file →
  `session import`): more moving parts, a credential-bearing temptation to
  "just add env to the export", and worse UX; rejected in favor of one
  atomic-enough verb. A file-based export may return later for backup
  purposes (roadmap I-119) with the same no-credentials rule.
- **Moving individual daemon sessions (PTYs) instead of worktree groups:**
  a bare PTY without its layout/tab context is rarely what "move my session"
  means in a worktree IDE; groups are the unit users see in the sidebar. A
  single-worktree group move covers the seed's per-session semantics.

## Security

- **Credential rule (the core):** nothing environment-, token-, or
  identity-shaped is read from or written to either store by the mover. The
  moved rows contain layout, titles, cwds, foreground-command strings, and
  scrollback snapshots — no composed env. Respawn in the target goes through
  the target profile's clear-then-allowlist composition, so the panes come up
  with target credentials or not at all. A test pins that the transferred
  column set never grows env-shaped columns without this spec changing.
- **What DOES cross, stated honestly:** `pane_cmds` (captured foreground
  argv — may embed secrets a user typed into a command line) and
  `scrollback_snapshot` (terminal output — may contain printed secrets)
  move with the group. That is the user's explicit, Admin-scoped choice; the
  dry-run names both. No redaction is attempted (silent redaction would be
  dishonest about what the target can read).
- **Scope:** `sessions.migrate` requires Admin; Admin capabilities never
  appear on MCP/plugin surfaces (existing catalog test) and this row is
  CLI-only besides. No network transport is involved; both stores are local
  files owned by the invoking user.
- **Kill consent:** live processes are never terminated without `--kill`;
  the refusal lists exactly which sessions are alive so the user knows what
  consent means.
- **Blast radius:** worst case is the duplicate-visible crash window (both
  profiles list the group until re-run) — chosen over any window where
  neither does. No writes outside the two profiles' DBs.

## Open questions

- Selector ergonomics: worktree path vs group name vs sidebar pick — v1
  accepts a worktree path (unambiguous); is a fuzzy group-name selector
  worth it?
- Terminal groups (`kind = terminal`, no worktree): allow moving them too?
  Nothing in the mechanics prevents it; lean yes, same rules, `pane_sessions`
  cleared likewise.
- Should the source keep a tombstone row ("moved to profile X on date") for
  the H-110 profile-audit story, or is the report output enough until an
  audit log exists? Lean report-only until H-110 lands.
