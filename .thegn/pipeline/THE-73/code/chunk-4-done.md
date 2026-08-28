# THE-73 · chunk 4 (REVISION) — completion summary

Branch `tg/the-73-sidebar-reap`. Implements `.thegn/pipeline/THE-73/code/chunk-4.md`
in full: a rendered registry row is now also _openable_, on both the warm-switch
and the same-workspace path.

Commits (both carry the spec's exact subject, matching the chunk-3 impl/test
split the architect review accepted):

- `0ae915b6` — implementation
- `04ecade7` — tests

## What changed

### 1. `session.rs` — the adoption loop is now one shared function

New free fn `adopt_registered_worktrees(db, cfg, session, slug, &mut worktrees)
-> HashMap<String, i64>` holds exactly what `resurrect_with_cfg` used to inline:
the `positions` capture, the `ambient_cache`, the `row_is_remote_effective` +
`is_dir` skip, and chunk 3's `adopt` predicate — verbatim, comments included.
`resurrect_with_cfg` now reads:

```rust
let positions = adopt_registered_worktrees(db, cfg, session, &slug, &mut worktrees);
```

and keeps its three-tier sort unchanged.

**Verified a pure extraction.** The whole chunk-4 diff deletes exactly two
things: the moved block, and the one `land_on(session, group);` line in the
same-workspace arm of `switch_workspace` (`git diff c79289f1..HEAD -- crates/
thegn-host/src | grep '^-'`). No pre-existing test line in `session.rs`,
`run_tests.rs` or `sidebar.rs` was edited, and every existing resurrect test —
including the four chunk 3 pinned — passes unedited (see Tests below).

New live-session entry point:

```rust
pub(crate) fn Session::adopt_missing_registered(&mut self, db, cfg) -> usize
```

Append-only by construction — it calls the shared fn and does **not** re-run
resurrect's sort, because `self.active` is an index and re-sorting a live
session would move the user's focus and reorder their tabs. The doc comment says
so, and also that appending matches resurrect's own tier-2 intent. Slug is
resolved from `self.id` via `repo_slug_with`, guarded on
`Path::new(&self.id).is_absolute()` so a non-path session id adopts nothing.

Also new: `Session::land_on_group(&mut self, name) -> bool` — `land_on`'s body
plus the bool the caller needs to tell "landed" from "no such group".

### 2. `run.rs` — two call sites inside `switch_workspace`, no logic

Warm arm, after `session.active = rw.active;` and before `land_on`:

```rust
session.adopt_missing_registered(db, cfg);
```

with the comment the spec asked for (one extra SELECT on a user-initiated
switch; the cold arm already pays strictly more; deliberately inline — no timer,
no thread, no channel).

Same-workspace early return, replacing the bare `land_on(session, group)`:

```rust
if let Some(name) = group
    && !session.land_on_group(name)
{
    session.adopt_missing_registered(db, cfg);
    session.land_on_group(name);
}
return true;
```

Five lines, all calls. Behaviour for `group == None` and for a group that
already exists is byte-identical to the old `land_on`.

### 3. `handlers/sidebar_activate.rs` — the residual case says why

After a `true` return from `switch_workspace`, when `group` was `Some(name)` and
the session still has no group by that name:

```
'{name}' isn't loaded — its registry row may be stale
```

Deliberately **not** the existing "Can't open workspace — … is gone or
unreadable" message (which would be false here); `switch_workspace`'s return
value is unchanged. Nothing after this point in `activate_row_target` writes
`model.status`, so the message survives to the frame.

## Tests

New, all passing:

`session.rs mod tests` —

1. `adopt_missing_registered_appends_a_row_the_live_session_lacks` — live session
   `{slug}/home` + `{slug}/zzz` (active = zzz), registry holds `{slug}/foo` in a
   temp dir outside any `worktrees_dir` with a non-matching recorded root.
   Asserts the exact resulting name order (foo **last**, the two pre-existing
   groups in their original order), the adopted path, and that
   `worktrees[active].name` is still `{slug}/zzz` — focus preserved **by name**,
   not by index.
2. `adopt_missing_registered_is_a_no_op_when_the_registry_matches` — returns 0,
   `worktrees` compares equal to the pre-call clone, `active` unmoved.
3. `adopt_missing_registered_ignores_a_non_path_session_id` — ids `"default"`
   and `""` adopt nothing, with a fixture-sanity assert that the registry row
   really does carry `db::session()` as its `session_name` (i.e. arm 1 _would_
   have matched an id of `"default"` without the `is_absolute` guard).

`run_tests.rs` —

4. `a_warm_switch_adopts_worktrees_registered_while_it_was_parked` — stashes
   workspace B (home only) into the pool, `put_worktree`s `{slug_b}/fresh`
   while it is parked, switches A→B warm, asserts `fresh` is a live group and
   that `land_on` focused it.
5. `activating_a_not_yet_loaded_group_of_the_active_workspace_lands_on_it` —
   `switch_workspace(target = session.id, group = Some(late))` leaves the
   session focused on the newly adopted group.

**Non-vacuity verified.** With the two `run.rs` calls removed (warm arm reverted
to plain `pool.stash`, early return reverted to `land_on(session, group)`), both
of the new `run_tests.rs` guards FAIL — the same-workspace one with
`left: "the73-active/home", right: "the73-active/late"`. `run.rs` was then
restored byte-exactly (`git diff crates/thegn-host/src/run.rs` empty against the
impl commit).

Test 4 is the item 4 the spec asked for in a different form: rather than a new
`resurrect_is_unchanged_by_the_extraction`, the extraction is proved by every
existing resurrect test staying untouched and green.

**Hermeticity.** `session.rs` tests use the module's existing `temp_db()` +
`temp_worktree_dir()`. `run_tests.rs` test 5 uses a temp on-disk DB and takes
`switch_workspace`'s early return, so it queues no `db_task` write. Test 4's
warm arm _does_ queue `persist_session_layout` + `set_active_workspace` on the
process-global `db_task` writer, which opens the **real** state DB — so that
test holds `ENV_LOCK` and redirects `XDG_STATE_HOME` via the module's
`XdgGuard`, matching `hydration_worker_loads_real_workspaces_into_sidebar`. No
git fixture is used, so the `commit.gpgsign` rule doesn't apply.

## Commands run

```
just quick thegn-host                                  # clean
cargo clippy -p thegn-host --tests                     # clean, no warnings
cargo nextest run -p thegn-host \
  -E 'test(session::) + test(resurrect) + test(adopt) + test(switch) + …'
  → 106 tests run: 106 passed, 0 failed
cargo nextest run -p thegn-host \
  -E 'test(a_warm_switch_adopts) + test(activating_a_not_yet_loaded) \
      + test(adopt_missing_registered) + test(resurrect)'
  → 17 tests run: 17 passed, 0 failed
```

No `just test` / `just ci` / `just coverage` / `just lint` / e2e; no
full-workspace compile.

## Guardrails

- **No new I/O at idle or on the render path.** The one new `db.worktrees()`
  SELECT fires only on a user-initiated workspace switch / row activation. No
  new thread, channel or wake source; `render_plan::plan` untouched.
- **No `thegn-core` change** — `git diff main...HEAD --name-only` matches
  nothing under `crates/thegn-core/`, so the 95% core gate cannot have moved.
- **No `test/*-ratchet.txt` modified**, and none needed: no colour/glyph
  literal, no `#[cfg]` outside `platform/`, no `gh` call, no `async fn` in a
  provider trait, no new `let _ = …`, no new idle poll.
- **No `docs/help/` change** — no new `ACTION_SPECS` action, keybind, zone or
  panel section.
- **No new logic in `run.rs`**: net +22/−1 lines there, of which 16 are comment
  and 5 are calls into `session.rs`.
- Strict widening: nothing that loaded before stops loading; the only behaviour
  removals are "silently do nothing" outcomes.

## Unverified

- **Frames may move.** After chunk 1 the sidebar renders union rows; after this
  chunk activating one can adopt a group, which changes the live tree and
  therefore the tabbar/sidebar. e2e is known-broken and out of scope per the
  spec — `test/muse/snapshots/` fixtures whose session and registry disagree may
  need a re-record.
- **Full-workspace gates not run** (budget): `just test`, `just lint`,
  `just coverage`, cross/MSRV/doc remain the Lead's pre-push gate. `just quick
thegn-host` and `cargo clippy -p thegn-host --tests` are clean.
- **`adopt_missing_registered_ignores_a_non_path_session_id`'s `"default"` case
  is env-sensitive.** It is non-vacuous only while `db::session()` returns
  `"default"` (i.e. `THEGN_SESSION` unset, the normal case). With
  `THEGN_SESSION` set to something else that half becomes vacuous; the `""` case
  is unconditional. Not worth an `ENV_LOCK` for.
- **The warm arm's extra SELECT is not benchmarked.** It is one
  `db.worktrees()` read per warm switch — argued rather than measured, on the
  grounds that the cold arm already pays a full resurrect plus a `db_task::flush`
  barrier. `just bench` not run (machine-dependent, excluded from `ci`).
- **The `db_task`-writer isolation in test 4 is process-scoped.** `XdgGuard`
  only redirects the writer if its `OnceLock` connection has not already been
  opened against the real DB earlier in the same process. That holds under
  nextest (process per test, and the gate) but not necessarily under plain
  `cargo test`. This is the pre-existing `wt new`/hermeticity gap the review
  logged as follow-up 3, not something this chunk introduces.
- **Windows path equality** (chunk 2's open question) is untouched and still
  unverified here; `check-cross` not run.
