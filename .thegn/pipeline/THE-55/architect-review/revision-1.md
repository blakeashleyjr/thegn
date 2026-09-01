# THE-55 Architect Revision 1

Status: REVISE

## Required corrections

### 1. Make dry-run strictly read-only

`crates/thegn-host/src/cmd/session_move.rs:102-109` opens both stores before the
dry-run branch. `Db::open_at` creates the parent directory and calls
`Db::init` (`crates/thegn-core/src/db.rs:314-318`), which can create, migrate,
or otherwise write the target database. The source open also uses the normal
startup path. This violates the design's no-write dry-run contract, especially
when the target profile has no database yet or has an old schema.

Expected fix: add an explicitly read-only/no-create database-opening path for
dry-run (including no startup prune/migration), and use it for both source and
target preflight. Add a regression test with an existing profile root and an
absent or stale target database, asserting that no database, WAL, journal, or
schema files are created or changed.

### 2. Preserve the actual default-profile state root

`crates/thegn-host/src/cmd/session_move.rs:465-479` reconstructs the default
database as `$HOME/.local/state/thegn/thegn.db` when the active source is named.
The profile startup path has already rerooted `XDG_STATE_HOME`, so this loses a
custom XDG state root (and can read/write the wrong default database). The
implementation must retain the pre-reroot default root or carry it through the
profile path resolver.

Expected fix: resolve the default target from the original configured/default
state root, not from the process-global state after named-profile rerooting.
Add a test using a custom `THEGN_DIR`/`XDG_STATE_HOME` with a named source and
default target, asserting the custom default database is selected and the
standard home directory is untouched.

### 3. Emit the required opaque-payload dry-run warning

The design requires dry-run output to warn that opaque pane commands,
scrollback, dispatch reports, and notes are carried unchanged and are not
included in the audit. The dry-run branch at
`crates/thegn-host/src/cmd/session_move.rs:126-138` only reports live-session
and `--kill` warnings, and `report` at `:402-442` has no stable equivalent for
human or JSON output.

Expected fix: add a stable warning/field for both output modes on every
dry-run, including when there are no live sessions, and test both human and
JSON reports without serializing the opaque payloads.

### 4. Synchronize the OpenSpec change

`openspec/changes/add-session-profile-migration/specs/profiles/spec.md:7-19`
does not describe the implemented sidebar UI, dispatch/notes remapping,
pin-state policy, target fingerprint/resume behavior, or exact source cleanup.
`openspec/changes/add-session-profile-migration/tasks.md:5-43` still leaves all
implementation tasks unchecked. Expand/prune the spec to match the approved
design and implementation, mark only completed tasks complete, then rerun
`openspec validate --all --strict`.

### 5. Add host orchestration tests behind a controllable seam

`crates/thegn-host/src/cmd/session_move.rs:122-228` hard-codes daemon control
calls, while its tests at `:494-570` cover only pane-ID parsing and audit
serialization. The required chunk-2 scenarios are therefore not exercised:
live-session refusal, kill-and-relist, unreachable daemon, target-first
ordering, read-back failure, cleanup retry/failure, notification warning, and
dry-run behavior.

Expected fix: introduce a narrow injected control seam (or extract an
equivalent deterministic protocol boundary), then add tests covering those
branches with isolated temporary state roots. Keep the real client as the
production implementation; do not add vendor-specific logic to core.
