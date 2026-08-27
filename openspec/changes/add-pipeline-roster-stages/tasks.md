# Tasks — pipeline roster stages

## 1. Schema v56 (thegn-core)

- [x] 1.1 `db_migrate::additive_schema`: four idempotent
      `ALTER TABLE agent_dispatches ADD COLUMN` statements — `stage TEXT`,
      `parent_id INTEGER`, `session_id TEXT`, `artifact_path TEXT` — each with
      the reason it exists (and why `artifact_path` is a pointer, not a payload).
- [x] 1.2 `SCHEMA_VERSION` 55 → 56 with its changelog entry in the `db.rs`
      doc comment; version stamp last, no data reset, no backfill.
- [x] 1.3 Upgrade test (`db_migrate.rs`, the v52 test's pattern): a v55-shaped
      `agent_dispatches` carrying a real running row opens through `Db::open_at`,
      the row survives with its status, the four columns read `None`, the new
      columns are writable, and the stamp reaches `SCHEMA_VERSION`. The existing
      migration-ladder drift gate (migrated schema == fresh schema) covers the
      fresh-DB side.

## 2. Struct + store (thegn-core)

- [x] 2.1 `AgentDispatch` gains the four `Option` fields, each
      `#[serde(default)]` so a pre-v56 payload still deserializes; round-trip +
      legacy-payload unit tests.
- [x] 2.2 `NewDispatch<'_>` params struct + `new(issue, worktree, agent)`;
      `put_agent_dispatch` takes it instead of positional arguments; every caller
      updated (`daemon/service.rs`, `handlers/tracker.rs`, core tests).
- [x] 2.3 All explicit column lists move together: one `DISPATCH_COLS` const +
      one `map_dispatch` row mapper shared by `list_dispatches` and
      `get_dispatch`, so a future column cannot land in one read and not the
      other. Test asserts the list read and the by-id read return the same row.
- [x] 2.4 `NotificationStore::dispatch_for_exit(worktree_path, session_id)`:
      session-id match first, else the most recent row whose typed status
      `is_active()`. `dispatch_for_worktree` / `dispatch_info_for_worktree` /
      `dispatch_dispatched_at_ms` keep their exact semantics for their other
      callers.
- [x] 2.5 Attribution tests: two active rows in one worktree resolve by session
      id (each to its own row); an unknown session id falls back; an empty
      session id is treated as none; the fallback skips terminal rows and
      returns `None` when every row is terminal; an unparseable status is
      skipped; an unknown worktree returns `None`.

## 3. Pane-exit attribution (thegn-host)

- [x] 3.1 `pty_drain.rs`: capture the dying pane's daemon session id before it
      leaves the pane table (a daemon-routed/adopted pane is a `Stream` pane
      whose session id the server announces; `None` for a local PTY pane) and
      switch the `Done`/`Failed` stamp from `dispatch_info_for_worktree` to
      `dispatch_for_exit`.
- [x] 3.2 Document the division of labour in code at that site: this handler
      stamps only workers that are panes; a headless session's terminal status is
      Lead-written after `sessions.wait`.

## 4. Wire, additive (thegn-svc)

- [x] 4.1 `DispatchPutReq` gains `stage` / `parent_id` / `session_id` /
      `artifact_path` as `#[serde(default)] Option`s; the HTTP handler already
      passes the whole struct through, so no per-field plumbing.
- [x] 4.2 `daemon/service.rs::dispatch_put` maps them onto `NewDispatch`;
      round-trip test asserting a plain put still yields all-`None` and a
      pipeline put returns every column on the row.
- [x] 4.3 **Assert UNCHANGED**: no new capability row, no new verb, no scope
      change — `capability.rs` and the control verb/route tables must be
      byte-unchanged in this change's diff (`git diff --stat` shows neither
      file). `dispatches.update` is deliberately not added: `put` carries every
      column.
- [x] 4.4 Regenerate `docs/api/control-v1.json` LAST
      (`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`)
      and verify the diff is additive only — four optional properties, no
      `required` change, no route change.

## 5. CLI (thegn-host)

- [x] 5.1 `thegn dispatch put <issue_id> <worktree_path> <agent_name>
[--stage --parent --session --artifact] [--json]`, DB-direct like its
      siblings. A `--parent` naming no row is refused before the insert.
- [x] 5.2 The human `dispatch list` table gains stage + parent columns
      (`-` when absent, so the shape stays aligned for a mixed roster).
- [x] 5.3 No `cli_help::GROUPS` change — `dispatch` is already the Forge
      group's noun. No new TUI action, keybind, zone or panel section, so
      `docs/help/` and the help ratchets are untouched.
- [x] 5.4 `thegn session open --adopt` → `OpenSpec.adopt` (was hardcoded
      `false`); default stays `false`. `--resume` explicitly deferred. The
      `adopt_session` intent has **no consumer** in the tree today (verified:
      `take_intents` is called for `focus_workspace` and `launch_preset` only),
      so the flag records the request and is inert at the UI until part 3 adds
      the drain — stated in design.md and in the flag's own help text rather
      than promised.
- [x] 5.5 Unit tests for the `put` path against an isolated `Db::open_at`
      (pipeline columns round-trip; a bad parent leaves no row), plus smoke
      coverage of `dispatch put --json`, the stage in `dispatch list`, and the
      bad-parent rejection.

## 6. Validation

- [x] 6.1 Scoped test runs: `cargo nextest -p thegn-core` (dispatch + migration + ladder families), `-p thegn-svc --test control_schema`, `-p thegn-host`
      (dispatch / session / pty_drain / daemon-service families); `just quick`
      per touched crate; `openspec validate --all --strict`; `treefmt`.
- [ ] 6.2 Run `just ci` once at the end — deferred to the serial land gate
      (full-workspace nextest + coverage + smoke), per the repo's dev-loop
      policy.
