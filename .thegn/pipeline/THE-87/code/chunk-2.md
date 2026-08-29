# THE-87 · Chunk 2 — `thegn-host`: warn the swallowed sidebar reads; recover lost repo paths from the registry

Issue: https://linear.app/blakeashley/issue/THE-87 · Design: `.thegn/pipeline/THE-87/architect/design.md` §2–§3 · HEAD `a65b42a3` (citations against this HEAD)

**Crate:** `thegn-host`. **Parallelizable:** yes — file-disjoint from chunk 1
(`thegn-core`) and chunk 3 (`handlers/sidebar_keys.rs` + `run.rs`); no logical
dependency on either. In particular: do NOT touch
`handlers/sidebar_keys.rs`, `run.rs`, or `run_tests.rs` (chunk 3 owns them).

## Files touched (exact paths)

| Path                                       | Change                                                                                                                                                                                                                                                                                                                                                               |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-host/src/hydrate.rs`         | (a) `workspace_list`'s silent `workspaces()` swallow (`:1313-1315`) → `match` with a `tracing::warn!` on `Err`; (b) `db_worktree_list`'s silent `worktrees()` swallow (`:1493`) → same treatment; (c) NEW pure `heal_workspace_paths` next to `merge_workspace_lists` (`:1350-1358`); (d) call it in `build_model` after both sidebar lists are built (`:2401-2416`) |
| `crates/thegn-host/src/hydrate_tests.rs`   | The four `heal_workspace_paths` corner tests + the end-to-end healed-tree test                                                                                                                                                                                                                                                                                       |
| `crates/thegn-host/src/handlers/switch.rs` | Call `heal_workspace_paths` in `refresh_tab_model` after the `merge_workspace_lists(prev, workspace_list(session, None))` at `:94-98`                                                                                                                                                                                                                                |

Nothing else. `sidebar.rs` (`gather_groups`, `DbWorktree`) is NOT touched —
the heal repairs the DATA, and every row rebuild (`SidebarState::rebuild`,
`run.rs:1298-1306`) already re-derives from the healed lists.

## Approach

1. **Warn #1 — the root cause's silent swallow** (`hydrate.rs:1313-1315`):
   restructure
   ```rust
   if let Some(db) = db && let Ok(rows) = db.workspaces() { … }
   ```
   into
   ```rust
   if let Some(db) = db {
       match db.workspaces() {
           Ok(rows) => { /* existing loop, unchanged */ }
           Err(e) => tracing::warn!(
               target: "thegn::hydrate",
               error = %e,
               "workspaces read failed during sidebar hydration — every \
                workspace degrades to a live fallback until the next \
                successful pass"
           ),
       }
   }
   ```
   Ok-path byte-identical. (The `Db::open` failure at `hydrate.rs:3303-3309`
   already warns — leave it.)
2. **Warn #2 — the registry swallow that defeats recovery** (`hydrate.rs:1493`):
   `db.worktrees().unwrap_or_default()` → collect the `Result`, log
   `tracing::warn!(target: "thegn::hydrate", error = %e, "worktree registry read failed — sidebar recovery and registered rows unavailable this pass")`
   and continue with an empty vec (same behaviour, now visible). Do NOT touch
   the other `worktrees().unwrap_or_default()` sites (`hydrate.rs:904`,
   `:1614`, `:4437`) — different features, out of scope (design §2).
3. **The heal** (pure, no I/O — safe on the loop):

   ```rust
   /// Recover a live-fallback workspace's lost repo root from the worktree
   /// registry: every `DbWorktree` of the slug carries its repo's root, so
   /// the first non-empty match repairs the entry. Pure, no I/O — hydration
   /// and the loop-side switch rebuild both run it. Idempotent; never
   /// touches entries that already carry a path.
   pub(crate) fn heal_workspace_paths(
       workspaces: &mut [(String, String, String, String)],
       db_worktrees: &[crate::sidebar::DbWorktree],
   ) -> usize
   ```

   - For each entry with an **empty** `repo_path` (index 3), find the first
     registry row with `row.slug == slug && !row.repo_path.is_empty()` and
     copy `row.repo_path` in; count heals; one `tracing::debug!` per healed
     slug (target `thegn::hydrate`, fields `slug`, `repo_path`).
   - No registry match ⇒ entry stays a live fallback (chunk 3's refusal
     guards that case; do not invent paths).

4. **Call site 1 — `build_model`** (`hydrate.rs:2401-2416`): make
   `sidebar_workspaces` `mut` and call
   `heal_workspace_paths(&mut sidebar_workspaces, &sidebar_db_worktrees);`
   after `db_worktree_list` runs. (Order inside the fn matters: the heal needs
   the registry, so it goes after `:2410`, before `FrameModel { … }`.)
5. **Call site 2 — `refresh_tab_model`** (`handlers/switch.rs:94-98`): after
   the merge assigns `model.sidebar_workspaces`, heal from the registry
   already in the model. Borrow discipline: `let registry =
std::mem::take(&mut model.sidebar_db_worktrees);` →
   `heal_workspace_paths(&mut model.sidebar_workspaces, &registry);` →
   `model.sidebar_db_worktrees = registry;` (or heal into a fresh `Vec` and
   assign — never two simultaneous mutable borrows of `model`). Zero I/O, so
   this stays on-loop-safe.
6. **Render-plan invariants**: the heal only changes model data between
   hydrations/switches. `hydration_eq` already compares
   `sidebar_workspaces` + `sidebar_db_worktrees` (`model_eq.rs:26-27`), so a
   healed list counts as a real change exactly once (one `Full` recompose when
   the tree changes, `Skip` when a later pass is identical). No new wake
   sources; no work on the idle path.

## Tests (scoped)

```
just quick thegn-host
cargo nextest run -p thegn-host heal
cargo nextest run -p thegn-host workspace_list
cargo nextest run -p thegn-host merge_
```

New tests in `hydrate_tests.rs` (follow the file's existing scratch-dir +
cleanup patterns):

- `heal_fills_a_lost_repo_path_from_the_registry` —
  `workspaces = [("app","app","repo","")]`, one `DbWorktree { slug: "app",
repo_path: "/r/app", … }` ⇒ path becomes `"/r/app"`, returns 1.
- `heal_leaves_db_backed_entries_alone` — a non-empty entry is never
  rewritten even when a registry row disagrees (the DB-backed list is
  authoritative when it has a path).
- `heal_is_idempotent` — running twice returns `1` then `0` and the list is
  unchanged after the second run.
- `heal_does_nothing_when_the_registry_is_empty` — the degraded
  `build_initial_model(None)` shape: empty registry ⇒ returns 0, entry stays
  empty (this pins the refusal-covered residual window).
- `healed_live_fallback_renders_its_registered_worktrees` — end-to-end:
  `sidebar::build_rows` over a session whose workspace entry was healed (real
  path) + ≥2 registered `DbWorktree` rows for the slug renders ≥2 worktree
  rows; the identical inputs with the UNhealed empty path render 0. This is
  the user-visible fix, and it documents why chunk 3's refusal still matters.

All existing `workspace_list` / `merge_workspace_lists` tests
(`hydrate_tests.rs:369-640`) must pass unmodified.

## Done-criteria

- `just quick thegn-host` clean; scoped nextest filters above green.
- `grep -n "let Ok(rows) = db.workspaces()" crates/thegn-host/src/hydrate.rs`
  returns nothing; `grep -n "worktrees().unwrap_or_default()"
crates/thegn-host/src/hydrate.rs` returns only `:904, :1614, :4437` (the
  out-of-scope trio) — `db_worktree_list` no longer swallows silently.
- `heal_workspace_paths` has no `Db`/I/O/subprocess reference (pure fn, pure
  tests).
- `git diff --stat` shows ONLY the three files listed above.

**Commit subject (exact):** `fix(hydrate): warn swallowed workspace reads; recover lost repo paths from the registry`
