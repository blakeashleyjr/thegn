# THE-73 · chunk 4 (REVISION) — a rendered registry row must also be _openable_

**Read first:** `.thegn/pipeline/THE-73/architect/design.md` §1 and §2/F1, then
`.thegn/pipeline/THE-73/architect-review/verdict.md` §"The gap". `CLAUDE.md`
(dev-loop policy, invariants, ratchets) is binding.

This is a revision chunk raised by the architect review of chunks 1–3. Chunks
1–3 are correct and stay; this closes the half of the repro they left open.

## Files touched (exact)

- `crates/thegn-host/src/session.rs` (impl + its inline `mod tests`)
- `crates/thegn-host/src/run.rs` — **two small edits inside `switch_workspace`
  only** (see steps 2 and 3). `run.rs` is a ratchet-pinned god-file: put every
  new line of logic in `session.rs` and call it from here.
- `crates/thegn-host/src/handlers/sidebar_activate.rs` — one status message.
- `crates/thegn-host/src/run_tests.rs` — new tests.

## The gap

F1 (chunk 1) made the sidebar render the **union** of the live session and the
registry, so a registered worktree the session missed no longer vanishes. It
renders with the same target the dormant branch has always used:
`RowTarget::Workspace { repo_path, group: Some(tab_name) }`.

For a **dormant** workspace that target is a real switch. For the **active**
workspace — a placement F1 created for the first time — it is a dead click:

`crates/thegn-host/src/run.rs:1988-1991`, `switch_workspace`:

```rust
if session.id == target {
    land_on(session, group);   // finds `g.name == name`, else does nothing
    return true;               // ...and reports success
}
```

`sidebar_activate.rs:101-118` only surfaces a status when `switch_workspace`
returns `false`, so activating an appended row for the active workspace does
**literally nothing**: no pane, no switch, no message. The codebase's own
comment at that call site names this the one outcome a user cannot diagnose.

And it is not a rare fallback — it is the reported repro's steady state, because
the live session goes stale by design:

`run.rs:2010-2028`, the **warm arm** of `switch_workspace`:

```rust
if pool.contains(target) {
    let rw = pool.take(target).expect("contains() just checked");
    session.worktrees = rw.worktrees;   // the tree as it was when parked
    session.active = rw.active;
    ...
    land_on(session, group);
    return true;                        // no DB read, no adoption
}
```

A warm restore replays the parked in-memory tree verbatim. Worktrees registered
while the workspace was parked — `thegn wt new` from another shell, which is
exactly THE-73's repro — never become live groups, and every switch back
re-parks the same stale tree. Only a cold start re-reads the registry (chunk 3's
`resurrect_with_cfg`), so the staleness survives for the life of the UI process.

**Verified against the live state DB** (2026-08-27): `worktrees` holds 12
`thegn/…` rows; `tab_groups` for session `/home/blake/code/thegn` holds 4. No
`reaping registry row` / `stale worktrees pruned` line appears anywhere in
`~/.local/state/thegn/logs/` — nothing was deleted, the live session is simply 8
rows behind. After chunks 1–3 those 8 rows render again; without this chunk they
are unclickable.

## Approach

### 1. Extract the adoption loop into a reusable `Session` method

`session.rs`, `resurrect_with_cfg`: the `if let Ok(wts) = db.worktrees() { … }`
block (the `positions` capture, the `row_is_remote_effective` + `is_dir` skip,
and the `adopt` predicate chunk 3 rewrote) moves into a free function or
associated fn, e.g.

```rust
/// Adopt every registry row of `session`'s workspace that `worktrees` does not
/// already carry, appending in registry `position` order. Shared by the cold
/// resurrect and the warm workspace restore so both see the same registry.
fn adopt_registered_worktrees(
    db: &Db,
    cfg: &thegn_core::config::Config,
    session: &str,
    slug: &str,
    worktrees: &mut Vec<WorktreeGroup>,
) -> std::collections::HashMap<String, i64>   // the `positions` map
```

`resurrect_with_cfg` calls it and keeps its existing three-tier sort. **This
must be a pure extraction** — every pre-existing `session.rs` test passes
unedited, including the four chunk 3 pinned. If one needs editing you changed
behaviour; stop and say so.

Then the live-session entry point:

```rust
/// Adopt registry rows this live session is missing, APPENDING them (no
/// re-sort). Returns how many were adopted.
pub(crate) fn adopt_missing_registered(&mut self, db: &Db, cfg: &Config) -> usize
```

**Append only — do not re-run resurrect's sort.** `self.active` is an _index_
into `self.worktrees`; re-sorting a live session would silently move the user's
focus to a different worktree and reorder tabs under their hands. Appending also
matches resurrect's own tier-2 intent (a newly registered worktree lands at the
bottom). Say this in the doc comment.

It resolves the slug itself with `repo_slug_with(db, Path::new(&self.id))`,
guarded on `Path::new(&self.id).is_absolute()` — a non-path session id ("default",
an empty id) must adopt nothing rather than mint a bogus slug.

### 2. The warm arm re-reads the registry

`run.rs`, inside `if pool.contains(target) { … }`, after `session.active =
rw.active;` and **before** `land_on(session, group)`:

```rust
// The parked tree is a snapshot; worktrees registered while this workspace
// was in the pool (`thegn wt new` from another shell) are only in the DB.
// The cold arm below re-reads the registry via `resurrect_with_cfg`; the
// warm arm must too, or a warm switch keeps replaying a stale tree (THE-73).
session.adopt_missing_registered(db, cfg);
```

One line of call, no logic in `run.rs`. It adds one `db.worktrees()` SELECT to a
warm switch, which previously did no DB read — that is deliberate and worth the
comment: the cold arm already pays strictly more (a full resurrect plus a
`db_task::flush` barrier), and a switch is a user-initiated event, not idle
work. Do **not** put it behind a timer, a thread or a channel.

### 3. Activating a not-yet-loaded group of the ACTIVE workspace

`run.rs`, the `session.id == target` early return: try `land_on`; if `group` was
`Some(name)` and no group matched, `adopt_missing_registered` and retry. Keep it
to a handful of lines — if it reads as logic rather than a call, move the body
into a `session.rs` helper (e.g. `Session::land_on_group(&mut self, name) -> bool`)
and call that twice.

`handlers/sidebar_activate.rs`: after a `true` return from `switch_workspace`,
when `group` was `Some(name)` and `session.worktrees` still has no group by that
name, set an accurate status — something like
`"'{name}' isn't loaded — its registry row may be stale"`. Do **not** reuse the
existing `"Can't open workspace — {repo_path} is gone or unreadable"` message;
it is false here and would send the user looking at the wrong thing. Leave
`switch_workspace`'s return value as it is.

### Guardrails

- **No new I/O on the render path or at idle.** The new DB read is on a
  user-initiated switch/activate only. No new thread, channel or wake source;
  `render_plan::plan` untouched.
- **No `thegn-core` change** — the core's 95% gate must not move.
- No colour/glyph literal, no `#[cfg]` outside `platform/`, no `gh` call, no
  `async fn` in a provider trait. Every new `let _ = …` needs a
  `// best-effort: <why>`. **Do not add an entry to any `test/*-ratchet.txt`.**
- No new `ACTION_SPECS` action, keybind, zone or panel section ⇒ no
  `docs/help/` change.
- Strict widening again: nothing that loads today stops loading, and no
  pre-existing test in `session.rs`, `run_tests.rs` or `sidebar.rs` may be
  edited.

## Tests to add

`session.rs` `mod tests` (beside chunk 3's THE-73 guards):

1. `adopt_missing_registered_appends_a_row_the_live_session_lacks` — live
   session with `{slug}/home` only, registry holding `{slug}/foo` (dir outside
   any `worktrees_dir`); assert `foo` is appended **last**, that the pre-existing
   groups keep their order, and that `active` still points at the same group
   **by name**.
2. `adopt_missing_registered_is_a_no_op_when_the_registry_matches` — returns 0
   and mutates nothing.
3. `adopt_missing_registered_ignores_a_non_path_session_id` — `id` of `"default"`
   / `""` adopts nothing.
4. `resurrect_is_unchanged_by_the_extraction` is not a new test — instead prove
   it by leaving every existing resurrect test untouched and green.

`run_tests.rs` (mirror the fixture style of
`palette_worktree_switch_persists_active_tab_for_target_workspace`, `:1490`):

5. `a_warm_switch_adopts_worktrees_registered_while_it_was_parked` — stash a
   workspace into the pool, `put_worktree` a new `{slug}/…` row, switch back,
   assert the new group is live and that `land_on` reaches it.
6. `activating_a_not_yet_loaded_group_of_the_active_workspace_lands_on_it` —
   `switch_workspace(target = session.id, group = Some(new_tab))` leaves the
   session focused on the newly adopted group instead of silently doing nothing.

**Test hermeticity (`CLAUDE.md`):** `Db::open_memory()` / `temp_db()` as the
neighbouring tests do, or isolate `XDG_STATE_HOME`; never touch the real state
DB. Any git fixture needs `-c commit.gpgsign=false`.

## Commands to run (scoped only)

```sh
just quick thegn-host
cargo clippy -p thegn-host --tests
cargo nextest run -p thegn-host -E 'test(session::) + test(resurrect) + test(adopt) + test(switch)'
```

**Do not run** `just test`, `just ci`, `just coverage`, `just lint`, `just e2e`,
or any full-workspace compile.

## Done criteria

- A warm workspace switch adopts registry rows registered while the workspace
  was parked; the cold arm's behaviour is unchanged.
- Activating a union row for the **active** workspace either lands on the
  worktree or says why it can't — never silently nothing.
- The adoption predicate lives in exactly one place, called by both arms.
- New tests pass; every pre-existing test passes unedited.
- `just quick thegn-host` and `cargo clippy -p thegn-host --tests` are clean.
- No ratchet file modified; no `thegn-core` file modified; no logic added to
  `run.rs`.
- Committed with exactly this subject:

  ```
  fix(session): adopt registry rows on a warm workspace switch (THE-73)
  ```

## Note for the coder

This chunk touches the sidebar's rendered output indirectly (rows that used to
be inert become live `RowTarget::Tab` rows after adoption). e2e is known-broken
and out of scope — do not run it; just say in your report that frames may move.
