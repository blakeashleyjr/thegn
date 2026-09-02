# THE-48 reconcile — merge current main into the lane

## Files to touch (exact paths)

- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-core/src/db.rs`
- `crates/thegn-core/src/db_migrate.rs`
- `crates/thegn-svc/src/control/client.rs`
- `docs/api/control-v1.json`
- `docs/help/configuration.md`

## Task

Run `git merge main` in this worktree, resolve the conflicts, and commit.
The merge is NOT already started — begin it yourself.

## Named reconcile items

- **Schema renumber is the important one.** This lane added a migration at
  v62. Main has since advanced to **SCHEMA_VERSION = 66** (THE-56 landed an
  additive `autopilot_runs` migration at v66). Renumber this lane's migration
  to the **next free version above main's**, keep main's whole ladder intact,
  and make sure `SCHEMA_VERSION`, the migration function name, its dispatch arm
  in the ladder, and any `verify_v*_schema` helper all agree. Do not renumber
  or reorder anything main already shipped.
- `config_validate.rs` holds a comment ladder of marked-enum definitions and a
  **pinned count**. Main's count has moved. Append this lane's entries with the
  next free numbers and set the count to main's value plus the number of marked
  enums this lane actually adds. `cargo nextest run -p thegn-core
marked_definition` is the gate for exactly this mistake.
- `docs/api/control-v1.json` and `docs/help/configuration.md` are generated or
  ratcheted corpora — keep both sides' entries, and never hand-write a section
  that is generated at runtime.
- `client.rs`: both sides add methods; keep both.

Default to **keeping both sides** unless one side deliberately DELETED
something the other kept; verify with `git log` before dropping any code.

## Verification required before you report

- `XDG_STATE_HOME=/home/blake/.superzej/pipeline-state RUSTC_WRAPPER= just quick thegn-core`
- `XDG_STATE_HOME=/home/blake/.superzej/pipeline-state RUSTC_WRAPPER= just quick thegn-host`
- `XDG_STATE_HOME=/home/blake/.superzej/pipeline-state RUSTC_WRAPPER= cargo nextest run -p thegn-core marked_definition db_migrate config_example`
- `XDG_STATE_HOME=/home/blake/.superzej/pipeline-state RUSTC_WRAPPER= cargo nextest run -p thegn-svc --test control_schema`
- `git diff --check`

The `XDG_STATE_HOME` prefix is REQUIRED on every command that may open thegn's
database. This lane changes the schema; run without it and the migration will
be applied to the RUNNING instance's live database, locking the supervisor out
of its own roster and emptying the user's sidebar.
