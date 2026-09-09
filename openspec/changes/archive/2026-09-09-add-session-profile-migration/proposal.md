# Move persisted worktree-session state between profiles

Linear: THE-55

## Why

Profiles deliberately isolate state, daemon sockets, config, credentials, and
identity. Users nevertheless need to re-home a worktree's persisted session
presentation after choosing a different profile. A running child cannot change
its inherited credential environment, so the supported operation is a cold,
explicit metadata migration rather than live PTY transfer.

## Delivered design

- `thegn session move <worktree> --to-profile <name> [--kill] [--dry-run]
[--json]` is cataloged as CLI-only `sessions.migrate` with Admin scope.
- The exact worktree selects all matching groups/tabs plus the bounded
  allowlist of worktree registration, group-scoped sidebar state,
  dispatches/notes, and the active session's running-pin state.
- Pane commands, cwd, scrollback, reports, artifact references, and notes move
  opaquely. Credentials, identity, config, queue state, whole-session focus,
  and unrelated rows never move.
- Any running source profile instance blocks the operation. Confirmed live
  daemon sessions also block unless `--kill` explicitly stops and re-lists
  them. Imported daemon/session identifiers are cleared.
- The target transaction commits first, is read back against a stable
  sanitized fingerprint, and only then is the exact source set deleted.
  Retry adopts an identical committed target and completes cleanup; divergent
  collisions fail before mutation.
- Dry-run is strictly read-only and both output modes include counts, blockers,
  collision state, and the opaque-payload warning without printing payloads.
- A target-daemon notification is best effort after confirmed completion.

## Non-goals

- Live PTY/process migration or moving worktrees on disk.
- Copying or merging profile credentials, identities, config, or tokens.
- Remote/MCP/plugin access, bulk migration, or a cross-profile daemon RPC.

## Evidence

Implemented by the session-move core/store planner and host command, with
catalog tests and hermetic cold/kill/collision/dry-run smoke coverage.
