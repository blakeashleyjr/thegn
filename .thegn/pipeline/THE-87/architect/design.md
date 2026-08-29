# THE-87 — Sidebar: a live-fallback workspace (empty `repo_path`) hides its worktrees and retargets 'new worktree' to the active tab's repo

Issue: https://linear.app/blakeashley/issue/THE-87 · Worktree `tg/the-87-live-fallback-workspace` · HEAD `a65b42a3`

All file:line citations are against this HEAD. **Doctrine in one line:** the
sidebar's tree model is _data_, so the fix is (a) stop silently swallowing the
two DB reads that degrade it, (b) repair the degraded data from the registry
the model already carries (a pure heal, no I/O), and (c) refuse — never
silently substitute another workspace's repo — when a row's repo still can't
be resolved. Plus one root-cause amortization in `thegn-core`: a binary older
than the on-disk schema must stop paying full-init on every open.

---

## 0. Ground truth (verified, not hypothesized)

| Mechanism                                                                                                                                                                                                                                                 | Evidence                                                                                                                                                                                                         |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --- | ----------------------------------------------------------------- |
| The live-fallback shape: `workspace_list` emits `(slug, display, "repo", "")` for a live tab prefix with no DB `workspaces` row                                                                                                                           | `crates/thegn-host/src/hydrate.rs:1339-1345` ("The empty repo_path marks this as a live fallback")                                                                                                               |
| Silent swallow #1: `if let Some(db) = db && let Ok(rows) = db.workspaces()` — an `Err` (e.g. SQLITE_BUSY) degrades **every** workspace to a live fallback at once, no log                                                                                 | `hydrate.rs:1313-1315`                                                                                                                                                                                           |
| Silent swallow #2 (recovery source): `db.worktrees().unwrap_or_default()` inside `db_worktree_list` — silent empty registry also defeats the heal                                                                                                         | `hydrate.rs:1493` (other `worktrees()` swallows at :904, :1614, :4437 are different features — out of scope)                                                                                                     |
| `Db::open` failure during hydration DOES log and falls back to `build_initial_model(&session, None)` → `workspace_list(session, None)` → all-live-fallback list, and the fallback model's `sidebar_db_worktrees` is **empty** (not in the struct literal) | `hydrate.rs:3303-3309` (warn), `:3354-3362` (fallback send), `:2073` (db `None`), `:2060-2105` (`build_initial_model`), `:3255` (`needs_fallback_send`)                                                          |
| Effect (1): `gather_groups` is gated on `!repo_path.is_empty()` — a live-fallback row contributes **none** of its registered worktrees (incl. `home`)                                                                                                     | `crates/thegn-host/src/sidebar.rs:1446` (guard, with a comment that pins today's behaviour), rows built at `:980-987` from `sidebar_workspaces` entries whose header path is also empty (`:966-969`)             |
| Effect (2): `cursor_repo_root` returns `None` for that row → sidebar `n` / menu fall through to `SidebarOutcome::Synthetic(Action::NewWorktree)`                                                                                                          | `crates/thegn-host/src/handlers/sidebar_keys.rs:150-175` (`cursor_repo_root`; `workspace_repo_path` filters empty at :138-148), `:739-748` (key), `:1186-1191` (menu)                                            |
| The generic `Action::NewWorktree` arm resolves the root from the cursor row **when resolvable**, else `session.active_group()` — the cross-workspace bug                                                                                                  | `crates/thegn-host/src/run.rs:20992-21031` (sidebar lookup + `.filter(                                                                                                                                           | p   | !p.is_empty())` at :21005, active-group fallback at :21012-21016) |
| **Same bug, second site**: the wizard composite `NewWorktree { name, sandbox, agent, base }` duplicates the identical resolution, same active-group fallback                                                                                              | `run.rs:18952-18990` (comment "Same repo-root resolution as Action::NewWorktree", fallback at :18972-18976)                                                                                                      |
| The sidebar-resolved consumer (`SidebarOutcome::NewWorktreeIn`) passes the root straight to `begin_worktree_wizard`                                                                                                                                       | `run.rs:7172-7185`                                                                                                                                                                                               |
| Recovery source: `FrameModel.sidebar_db_worktrees: Vec<DbWorktree>`, each carrying `slug` + `repo_path` (the registered repo root)                                                                                                                        | `sidebar.rs:528-549` (`repo_path` at :534), `chrome.rs:458`; populated in `build_model` from `db_worktree_list` (`hydrate.rs:2410`, `:1486-1560` — `repo_path: w.repo_root.clone()` at :1565)                    |
| The registry and the workspace list are both plain model data compared by the idle guard, and rows rebuild from them at every `SidebarState::rebuild`                                                                                                     | `model_eq.rs:26-27`; `run.rs:1298-1306` (`build_rows(session, &model.sidebar_workspaces, …, &model.sidebar_db_worktrees, …)`)                                                                                    |
| The loop-side switch rebuild already merges prev + live: keeps non-empty DB-backed entries, re-derives fallbacks — the natural second heal point                                                                                                          | `crates/thegn-host/src/handlers/switch.rs:84-127` (merge at :94-98)                                                                                                                                              |
| Stale generations are dropped; the fallback model replaces `model` wholesale (loop-owned fields carried explicitly; the sidebar fields are NOT carried)                                                                                                   | `run.rs:9120-9200` (model_rx drain; carry-over block :9136-9152)                                                                                                                                                 |
| Schema gate: `OpenMode::Fast` only on **exact** `user_version` match (`SCHEMA_VERSION = 60`); anything else runs the full path — the ~45-statement `BEGIN; CREATE TABLE IF NOT EXISTS …` DDL transaction + ALTER probes under the WAL write lock          | `crates/thegn-core/src/db.rs:130`, `:243-255`, `:337-343`, `:386-389+`, user_version stamped at the END (`:920-928`)                                                                                             |
| The newer-schema warn fires on **every** `Db::init` that sees a newer file — and `Db::open()` is called from 356 thegn-host sites (per-hydration, per-prefetch, per-menu action), hence ~74k warns                                                        | `crates/thegn-core/src/db_migrate.rs:23-32` (`tracing::warn!` inside `detect_newer_schema`); the host surfaces the mismatch itself once at startup (`handlers/startup.rs:254-258`, consumed at `run.rs:758`)     |
| Additive-schema discipline that makes newer-tolerance safe: migrations only add tables/columns; the DDL batch is `IF NOT EXISTS` + idempotent ALTER probes; older builds read only columns they name                                                      | `db.rs:355-392` (drop-and-recreate caches keep table names), `db_migrate.rs` header; the project's own doc: "the additive schema is forward-compatible" (`db_migrate.rs:17-22`)                                  |
| Contract this restores                                                                                                                                                                                                                                    | `openspec/specs/sidebar/spec.md:12` ("Workspace/worktree tree model" — the sidebar SHALL render workspaces and their worktrees); `openspec/specs/state-db/spec.md:12-30` (single versioned store; DB is a cache) |

Degradation shape, end to end: one failed `Db::open` (or one failed
`workspaces()` read) during a hydration pass → `sidebar_workspaces` becomes
all-live-fallback (empty `repo_path` per slug) → every workspace renders only
its live tabs (registered + home rows vanish) → pressing `n` (or the menu, or
the palette action) on a workspace row falls through to
`session.active_group()` → **a worktree for the wrong repo** (`tg/lively-blade`
under mysage when the user asked for pantheon). Recovery today is only "the
next successful hydration heals the list" — the harm lands inside that window.

---

## 1. Fix (5) first — the aggravator: `open_mode` tolerates a newer schema; warn once

The v57-binary-vs-v60-DB case makes every `Db::open` take the full-init path
(the 45-statement DDL transaction under the WAL write lock, plus ALTER probes
and prune) — which is what raises the odds of the `workspaces()` read failing
in the first place. Two changes in `thegn-core`:

1. **`open_mode` becomes `on_disk >= current → Fast`** (`db.rs:250-255`).
   Correctness argument: `user_version` is stamped only after a full init
   completes (`db.rs:920-928`), so `on_disk >= current` proves the schema batch
   ran; the schema is additive by construction (the DDL is `IF NOT EXISTS` +
   idempotent ALTER probes; migrations only add tables/columns or drop-and-
   recreate _cache_ tables under the same names), so an older binary's reads
   and writes are unaffected by columns it doesn't know. `on_disk < current`
   stays `Full` (a migration is genuinely due). `open_memory` (fresh, ver 0)
   is unchanged.
2. **The fast path still records the mismatch**: the early return at
   `db.rs:337-343` sets `schema_mismatch: detect_newer_schema(ver,
SCHEMA_VERSION)` instead of hard `None` — exact match ⇒ `None` (no change),
   newer ⇒ `Some(ver)`, so the existing one-time startup status
   (`handlers/startup.rs:254`, `run.rs:758`) keeps working.
3. **The warn moves off the hot path and dedups per process**: delete the
   `tracing::warn!` from `detect_newer_schema` (`db_migrate.rs:29-31`) so the
   classifier stays pure, and emit it at the `db.rs::init` site behind a
   `static WARNED: std::sync::Once` — same target (`thegn::db`), same message.
   356 call sites × every wake ⇒ **1** line per process.

Doc comments that pin the old contract get updated in the same commit:
`db.rs:237-243` ("Anything else … is `Full`"), `db.rs:329-336` (fast-path
safety argument), and the exhaustive test `db_tests.rs:3017-3032`.

## 2. Fix (3) — log the swallows that degrade the tree

- `hydrate.rs:1313-1315` (`workspaces()`): restructure the `if let … && let
Ok(...)` into `match db.workspaces()`; on `Err(e)`:
  `tracing::warn!(target: "thegn::hydrate", error = %e, "workspaces read failed during sidebar hydration — every workspace degrades to a live fallback until the next successful pass")`.
  Ok-path unchanged.
- `hydrate.rs:1493` (`db_worktree_list`'s `worktrees()`): same treatment —
  this is the registry the recovery in §3 reads, so a silent empty here
  silently disables the heal.
- The `Db::open` failure at `hydrate.rs:3303-3309` already warns — cited, no
  change.
- The other `worktrees()` swallows (`hydrate.rs:904, :1614, :4437`) serve
  reconcile/activity/aggregate, not the sidebar tree — out of scope, left
  alone (the diff stays reviewable).

Cadence note: these sit on the hydration thread at the model cadence, so a
_persistently_ broken DB warns repeatedly — that is correct signal (the
always-on diagnostics ring is bounded), and §1 removes the realistic cause of
persistent failure.

## 3. Fix (2) — recover the lost repo root from the registry (pure heal)

One new pure function in `hydrate.rs`, next to `workspace_list` /
`merge_workspace_lists` (same data domain; `hydrate.rs` is not a ratcheted
god-file, and `run.rs` gets no new logic):

```rust
/// Recover a live-fallback workspace's lost repo root from the worktree
/// registry: every `DbWorktree` of the slug carries its repo's root, so the
/// first non-empty match repairs the entry. Pure, no I/O — safe on the loop.
pub(crate) fn heal_workspace_paths(
    workspaces: &mut [(String, String, String, String)],
    db_worktrees: &[crate::sidebar::DbWorktree],
) -> usize
```

Semantics: for each entry with an **empty** `repo_path` whose `slug` matches a
registry row's `slug` with a non-empty `repo_path`, copy the root in; count
heals; `tracing::debug!` per healed slug. Never touches non-empty entries;
never invents a path (no registry match ⇒ entry stays a live fallback ⇒ the
refusal in §4 guards it). Idempotent by construction.

Applied at exactly the two sites where both lists enter the model:

1. **`build_model`** (`hydrate.rs:2401-2416`): after
   `sidebar_workspaces = workspace_list(session, Some(db))` and
   `sidebar_db_worktrees = db_worktree_list(db, &app_cfg)` — heals the
   SQLITE_BUSY-shaped degradation where the registry read succeeded but the
   `workspaces` read failed, and any pass where the DB row is simply not there
   yet while worktrees are registered.
2. **`refresh_tab_model`** (`handlers/switch.rs:94-98`): after the
   `merge_workspace_lists(prev, workspace_list(session, None))` — heals the
   list from the registry already in `model.sidebar_db_worktrees` (pure, zero
   I/O, on-loop-safe). Borrow note: take the registry out of `model` first
   (`std::mem::take`), heal, put it back — or heal into a fresh `Vec` and
   assign; do not hold two mutable borrows of `model`.

Why the degraded _fallback_ model (`build_initial_model(&session, None)`,
`sidebar_db_worktrees` empty) is NOT healed: there is nothing to heal from —
carrying the previous model's registry across the swap would mean new loop-side
merge logic in `run.rs` for a window §1 + the next hydration already close.
Residual window: between a fallback model and the next successful hydration, a
live-fallback row with an empty registry renders without its registered rows
and refuses `n` with a status message instead of building in the wrong repo.
Refusal is the safety net; the heal is the recovery; §1 shrinks how often the
window opens at all.

Render-plan invariants: the heal only changes model _data_ before rows are
built — `SidebarState::rebuild` re-derives rows from the healed lists
(`run.rs:1298-1306`), and `hydration_eq` already compares both fields
(`model_eq.rs:26-27`), so a healed list correctly counts as a change (one
`Full` recompose when the tree actually changes, `Skip` when it doesn't). No
new wake sources, no on-loop I/O, no chrome recompose from pane output.

## 4. Fix (1) — never silently cross workspaces: refuse

One resolution helper on `SidebarState` in `handlers/sidebar_keys.rs` (beside
`cursor_repo_root`, which it wraps — no duplicated slug logic), consumed by
both run.rs sites and by the sidebar key/menu arms:

```rust
/// What `Action::NewWorktree` should do, given the sidebar cursor.
pub(crate) enum NewWorktreeTarget {
    /// The cursor row's repo resolved — open the wizard there.
    Root(String),
    /// The cursor row names a workspace/worktree/folder but its repo is
    /// unresolvable (live fallback, registry heal also failed). Refuse with a
    /// status message — NEVER fall back to another workspace's repo.
    Refuse(&'static str),
    /// No sidebar row in play (no sidebar focus, no selected row, or a
    /// terminals-region row): the active tab's repo is the intent, as before.
    ActiveFallback,
}
pub(crate) fn new_worktree_target(
    &self, model: &FrameModel, sidebar_focus: bool,
) -> NewWorktreeTarget
```

Mapping (order matters):

- `!sidebar_focus` **or** `selected_row` is `None` ⇒ `ActiveFallback`
  (preserves today's Alt+w-without-sidebar and palette behaviour — creating in
  the active tab's repo is the intended semantic there);
- `cursor_in_terminals` ⇒ `ActiveFallback` (both existing pre-guards — the
  sidebar `n` arm and the run.rs `Action::NewWorktree` guard arm — still
  convert terminals rows to NewTerminal before the helper runs; this arm is
  defensive only);
- `Workspace | Worktree | Folder` row ⇒ `cursor_repo_root(model)`:
  `Some` ⇒ `Root`, `None` ⇒ `Refuse`.

Refusal message (static, actionable): e.g.
`"No repo path for this workspace yet — it registers on the next refresh"`.
Exact wording is the coder's; it must name the cause and the remedy and must
NOT suggest the action moved elsewhere.

Consumers rewritten:

1. **Sidebar key `n`** (`sidebar_keys.rs:739-748`) and **menu "new-worktree"**
   (`:1186-1191`): `None ⇒ Synthetic(Action::NewWorktree)` becomes
   `None ⇒ Refuse` — set `model.status`, mirror `Id::Delete`'s fall-through to
   the tail `self.sync(model)` + `SidebarOutcome::Redraw` (an Essential-tier
   key never silently no-ops). Today the synthetic only ever produced the
   cross-workspace fallback at run.rs — the terminals case is handled before
   this arm — so refusing strictly narrows behaviour to the safe case.
   Extract the arm body into `fn new_worktree_outcome(&self, model) ->
SidebarOutcome` so both call sites share it and tests can drive it without
   keymap chord resolution.
2. **`run.rs:20992-21031` (`Action::NewWorktree`)**: replace the inline
   sidebar lookup + `unwrap_or_else(active_group)` with the helper —
   `Root(root)` keeps the existing `main_worktree` normalization +
   `begin_worktree_wizard`; `Refuse(msg)` sets `model.status`, `dirty = true`;
   `ActiveFallback` keeps today's `session.active_group()` → `main_worktree`
   → `current_dir` ladder and the `thegn_core::msg::warn("new-worktree: not
inside a git repository")` tail.
3. **`run.rs:18952-18990` (composite `NewWorktree { … }`)**: same helper, same
   three arms, wizard opened via `begin_worktree_preset` as today. This site
   is the issue's bug shape too and was missed by "run.rs ~20946" — one
   helper, both holes closed.

No new action ids, no keymap/spec changes, no help-page ratchet impact (the
help ratchet claims _actions_, and `Id::NewWorktree`/`Action::NewWorktree`
already exist); no config keys; no e2e snapshot change expected — the only
visible deltas are (a) previously-vanished rows reappearing after a heal and
(b) a status line on refusal, neither of which the committed cases exercise.
`test/ignored-result-ratchet.txt` is untouched (no new ignored `Result`s; the
two warns replace silent `let Ok(...)` / `unwrap_or_default()` shapes).

---

## 5. Regression tests (the issue's names, verbatim, plus the gaps)

| Test                                                                                                                                                | Where                                                                                              | Locks                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| --------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cursor_repo_root_is_none_for_a_live_fallback_workspace_row`                                                                                        | `sidebar_keys.rs` tests (~:1271 pattern)                                                           | A `Workspace` row whose workspace entry has an empty repo path resolves to `None` — the refusal precondition, pinned                                                                                                                                                                                                                                                                                                                                                  |
| `live_fallback_workspace_renders_none_of_its_registered_worktrees`                                                                                  | `sidebar.rs` tests (~:2798 pattern)                                                                | Same `db_by_slug` registry rows, two lists: a real `repo_path` renders ≥2 worktree rows, an empty one renders 0 — the effect-(1) shape                                                                                                                                                                                                                                                                                                                                |
| `heal_fills_a_lost_repo_path_from_the_registry` / `_leaves_db_backed_entries_alone` / `_is_idempotent` / `_does_nothing_when_the_registry_is_empty` | `hydrate_tests.rs`                                                                                 | §3 semantics, all four corners                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `healed_live_fallback_renders_its_registered_worktrees`                                                                                             | `hydrate_tests.rs` (end-to-end via `build_model`'s inputs, or `sidebar.rs` tests via `build_rows`) | A degraded list + populated registry renders the workspace's registered worktrees again (the user-visible fix)                                                                                                                                                                                                                                                                                                                                                        |
| `new_worktree_key_refuses_a_live_fallback_workspace_row`                                                                                            | `sidebar_keys.rs` tests                                                                            | `new_worktree_outcome` on such a row sets a refusal status + `Redraw`, and does NOT return `Synthetic(Action::NewWorktree)`                                                                                                                                                                                                                                                                                                                                           |
| `new_worktree_target_maps_focus_rows_and_fallbacks`                                                                                                 | `sidebar_keys.rs` tests                                                                            | No focus ⇒ `ActiveFallback`; resolvable row ⇒ `Root`; unresolvable repo-ish row ⇒ `Refuse`                                                                                                                                                                                                                                                                                                                                                                            |
| `open_mode_is_fast_on_current_or_newer_schema` (renames `open_mode_is_fast_only_on_exact_version_match`)                                            | `db_tests.rs:3017`                                                                                 | `0 → Full`, `current-1 → Full`, `current → Fast`, `current+1 → Fast`                                                                                                                                                                                                                                                                                                                                                                                                  |
| `newer_db_takes_the_fast_path_and_still_serves_reads_writes`                                                                                        | `db_tests.rs` (beside `fast_reopen_round_trips…`)                                                  | Open a file DB, write, bump `PRAGMA user_version` to `SCHEMA_VERSION + 1`, reopen ≥2×: reads/writes work, `schema_mismatch() == Some(v)` every time, and the `thegn::db` "newer than this build" line appears **at most once** in `diagnostics::ring_snapshot()` (the always-on WARN+ ring makes this order-independent; if the harness proves the ring unpopulated in unit tests, keep the mismatch round-trip assertions and note the warn-once as review-verified) |
| `detect_newer_schema_flags_only_a_newer_db`                                                                                                         | `db_migrate.rs:632`                                                                                | Unchanged (the classifier is now pure — return values identical)                                                                                                                                                                                                                                                                                                                                                                                                      |

Scoped commands only (dev-loop policy): `just quick <crate>` per crate touched
(`thegn-core` / `thegn-host`) + `cargo nextest run -p <crate> <filter>` for the
tests above. No full-workspace gates in-chunk; the pre-push hook owns those.

## 6. Chunking (file-disjoint; the Lead parallelizes)

| Chunk | Crate        | Files                                                  | Depends on                    |
| ----- | ------------ | ------------------------------------------------------ | ----------------------------- |
| 1     | `thegn-core` | `db.rs`, `db_migrate.rs`, `db_tests.rs`                | none                          |
| 2     | `thegn-host` | `hydrate.rs`, `hydrate_tests.rs`, `handlers/switch.rs` | none (independent of 1 and 3) |
| 3     | `thegn-host` | `handlers/sidebar_keys.rs`, `run.rs`, `run_tests.rs`   | none (independent of 1 and 2) |

No file overlap; logically independent (the refusal is correct without the
heal, the heal is useful without the refusal, and §1 stands alone). If run
serially: 1 → 2 → 3 is the natural order (root cause, recovery, guardrail).
`run_tests.rs` is listed in chunk 3 only as a placeholder for any
helper-level test the coder chooses to add there — chunk 2 and chunk 3 must
not both touch it.

## 7. Out of scope (deliberate)

- The other `worktrees()` swallows (`hydrate.rs:904, :1614, :4437`) —
  different features, no sidebar effect.
- Carrying the previous model's `sidebar_db_worktrees` across a fallback-model
  swap (needs loop-side merge logic in `run.rs`; the residual window is safe —
  refusal — and self-heals on the next hydration).
- Which route emptied `repo_path` historically (unpinned by the issue; both
  candidates now log or already logged).
- `open_mode` for `on_disk < current` (a genuine migration must keep running).
