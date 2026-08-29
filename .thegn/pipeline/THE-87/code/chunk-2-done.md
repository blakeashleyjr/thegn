# THE-87 · Chunk 2 — Completion Summary

**Commit:** `ee04cf9e` — `fix(hydrate): warn swallowed workspace reads; recover lost repo paths from the registry`

**Branch:** `tg/the-87-live-fallback-workspace`

## What was done

### hydrate.rs

1. **`workspace_list`** (~line 1313): Restructured the silent `db.workspaces()` Ok-pattern swallow into a `match` that warns on `Err` with `tracing::warn!(target: "thegn::hydrate", ...)`. The Ok-path is byte-identical.

2. **`db_worktree_list`** (~line 1535): Replaced `db.worktrees().unwrap_or_default()` with a `match` that warns on `Err` and continues with an empty vec — same behaviour, now visible.

3. **`heal_workspace_paths`** (new, after `merge_workspace_lists`): Pure function that recovers a live-fallback workspace's lost `repo_path` from the worktree registry. For each entry with empty `repo_path` (index 3), finds the first registry row with matching slug + non-empty repo_path and copies it in. Returns the count of heals. Idempotent; never touches entries that already carry a path.

4. **`build_model`** (~line 2455): `sidebar_workspaces` is now `let mut`; calls `heal_workspace_paths(&mut sidebar_workspaces, &sidebar_db_worktrees)` after `db_worktree_list` runs and before `FrameModel { … }`.

### handlers/switch.rs

5. **`refresh_tab_model`** (~line 94): After the merge assigns `model.sidebar_workspaces`, takes the registry via `std::mem::take`, calls `heal_workspace_paths`, then restores it. Zero I/O; loop-safe.

### hydrate_tests.rs

6. Five new tests, all passing:
   - `heal_fills_a_lost_repo_path_from_the_registry`
   - `heal_leaves_db_backed_entries_alone`
   - `heal_is_idempotent`
   - `heal_does_nothing_when_the_registry_is_empty`
   - `healed_live_fallback_renders_its_registered_worktrees` (end-to-end via `sidebar::build_rows`)

## Done-criteria verification

- `just quick thegn-host` — clean
- `cargo nextest run -p thegn-host heal` — 38 passed
- `cargo nextest run -p thegn-host workspace_list` — 3 passed
- `cargo nextest run -p thegn-host merge_` — 61 passed
- `grep "let Ok(rows) = db.workspaces()" hydrate.rs` — returns nothing ✓
- `grep "worktrees().unwrap_or_default()" hydrate.rs` — returns only `:904, :1668, :4495` (out-of-scope trio) ✓
- `heal_workspace_paths` has no Db/I/O/subprocess reference (pure fn) ✓
- `git diff --stat` shows only the 3 listed files ✓

## Files touched (exactly as specified)

| Path                                       | Change                                                                 |
| ------------------------------------------ | ---------------------------------------------------------------------- |
| `crates/thegn-host/src/hydrate.rs`         | Two warn restructures + `heal_workspace_paths` + call in `build_model` |
| `crates/thegn-host/src/hydrate_tests.rs`   | 5 new tests                                                            |
| `crates/thegn-host/src/handlers/switch.rs` | Heal call in `refresh_tab_model`                                       |

## Unverified

- No `just test` or `just ci` full-workspace run (per dev-loop policy)
- No e2e run
- The `healed_live_fallback_renders_its_registered_worktrees` end-to-end test assertion was adjusted from "0 worktree rows unhealed" to "unhealed < healed" because the session's live group always renders a worktree row regardless of repo*path emptiness — the `gather_groups` guard at `sidebar.rs:1453` blocks registry rows but not live session groups. This matches the actual semantics: a live-fallback workspace with the active session open \_does* show its own worktree; what was missing was the _registered_ worktrees of that repo.
