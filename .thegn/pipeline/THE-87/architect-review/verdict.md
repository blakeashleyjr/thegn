# THE-87 — Architecture review verdict: REVISE

Reviewer: ARCHITECT
Branch: `tg/the-87-live-fallback-workspace` (HEAD `49701371`)
Base: `main` (already merged; no conflicts)
Verdict: **REVISE**
Lane: `.thegn/pipeline/THE-87/architect-review/chunk-3-revise.md`

## Scope checked

`git diff main...HEAD` — 6 source files in 2 crates + 7 lane docs:

| File                                             | Δ (add/del) | Chunk |
| ------------------------------------------------ | ----------- | ----- |
| `crates/thegn-core/src/db.rs`                    | +36 / -16   | 1     |
| `crates/thegn-core/src/db_migrate.rs`            | +4 / -13    | 1     |
| `crates/thegn-core/src/db_tests.rs`              | +66 / -7    | 1     |
| `crates/thegn-host/src/handlers/sidebar_keys.rs` | +176 / -11  | 3     |
| `crates/thegn-host/src/handlers/switch.rs`       | +6 / -0     | 2     |
| `crates/thegn-host/src/hydrate.rs`               | +63 / -5    | 2     |
| `crates/thegn-host/src/hydrate_tests.rs`         | +150 / -0   | 2     |
| `crates/thegn-host/src/run.rs`                   | +142 / -103 | 3     |

Every diff hunk is file-localized to the chunk plan in
`architect/design.md` §6 — no file overlap between chunks, no
out-of-scope files touched. `git diff --check` clean.

## Verification gate

| Filter                                                                                                                                   | Result                                                                  |
| ---------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| `cargo nextest run -p thegn-core -E 'test(env_overlay) \| test(config_example) \| test(control_schema) \| test(db)'`                     | **495/495 pass** (3082 skipped, 0 failed)                               |
| `cargo nextest run -p thegn-host -E 'test(complete) \| test(help) \| test(catalog_tests) \| test(platform_ratchet) \| test(sidebar)'`    | **355/355 pass** (2291 skipped, 0 failed)                               |
| THE-87 named tests (`new_worktree`, `heal_`, `healed_live_fallback`, `cursor_repo_root`, `open_mode`, `newer_db`, `detect_newer_schema`) | **13/13 pass** (incl. the 3 issue-named regression tests)               |
| `just quick thegn-core` / `just quick thegn-host`                                                                                        | clean (per chunk done-docs; no full-workspace gate per dev-loop policy) |
| `git merge main`                                                                                                                         | already up to date (no conflicts)                                       |

## Findings

### P0 — none

### P1 — third NewWorktree cross-workspace hole (REVISION REQUESTED)

**Where:** `crates/thegn-host/src/run.rs:20457`
**Action:** `Action::NewWorktreeFromTemplate`

The design §4 explicitly listed "one helper closes both holes; no
third copy of the resolution may remain" as a hard invariant. Two of
the three sites were closed in commit `49701371`; this third site
slipped because chunk 3's grep
`grep -n "sidebar_repo" crates/thegn-host/src/run.rs` only catches the
two closed sites and missed this one — the template site walks
`model.sidebar_workspaces.iter().find(...)` instead.

The bug shape is identical to the two closed sites: the inline
resolution filters with `!p.is_empty()` and falls back to
`session.active_group().path` (the active tab), so a user clicking on
a live-fallback workspace would have the template-driven worktree
silently created in the _active tab's_ repo, not the workspace they
clicked. The site's own comment is candid about this:
"Resolve the repo root the same way NewWorktree does".

The revision chunk at
`.thegn/pipeline/THE-87/architect-review/chunk-3-revise.md` rewrites
the single arm to use the existing
`sb.new_worktree_target(&model, focus.sidebar())` helper with the
same three-arm mapping as `Action::NewWorktree`. One file touched
(`run.rs`); ~18 net lines; no test change required (the helper is
already unit-tested via
`new_worktree_target_maps_focus_rows_and_fallbacks`).

Note: the revision intentionally does NOT add a `current_dir` fallback
to the `ActiveFallback` arm of the template action. The template
picker is an explicit "active workspace" gesture; the
`current_dir` tail added to `Action::NewWorktree` in chunk 3 was a
different semantic (an unsourced Alt+w). Documented in
`chunk-3-revise.md` "Out of scope".

### Per-design verification (everything else passes)

- **§1 (`open_mode >=` fast path; warn once)** — `db.rs:256` is the
  new `on_disk >= current` predicate; the exhaustive test at
  `db_tests.rs:3017` covers all four regions including
  `SCHEMA_VERSION + 1 → Fast`. The `static MISMATCH_WARNED: Once` is
  at the `init` fast-path return site (`db.rs:347`) — function-local
  statics are still process-singletons in Rust (initialized once at
  program start, same `.bss` slot per binary), so the per-process
  semantic the design called for holds. The warn-once ring assertion
  is left as a `ring_snapshot()` flakiness concern and
  review-verified, per the chunk-1 done-doc. **OK**.
- **§2 (warn the two silent swallows)** —
  `hydrate.rs:1308-1325` (`workspace_list`'s `db.workspaces()`)
  matches on `Err` with `tracing::warn!` (target `thegn::hydrate`,
  same wording as the design); `hydrate.rs:1528-1541`
  (`db_worktree_list`'s `db.worktrees()`) is the same shape. The
  other three `worktrees().unwrap_or_default()` sites (`hydrate.rs:904,
:1668, :4495` — line numbers shifted post-edit) are explicitly
  out-of-scope and untouched. The `Db::open` failure warn at
  `hydrate.rs:3303-3309` is left as-is. **OK**.
- **§3 (heal)** — `heal_workspace_paths` at `hydrate.rs:1382` is
  pure, no I/O; called at `build_model:2468` and
  `refresh_tab_model` (`handlers/switch.rs:96-99` with the
  `std::mem::take` borrow pattern the design called for). The cheap
  first-frame `build_initial_model` is correctly untouched (residual
  window is guarded by refusal; design §3 "carrying the previous
  model's `sidebar_db_worktrees` across a fallback-model swap" is
  out of scope). **OK**.
- **§4 (refuse)** — `NewWorktreeTarget` enum, `NEW_WORKTREE_REFUSAL`
  constant, and `new_worktree_target` / `new_worktree_outcome` at
  `sidebar_keys.rs:131-228`. The key `n` arm (`:792`) and the menu
  arm (`:1233`) both route through `new_worktree_outcome`. The
  `Action::NewWorktree` arm at `run.rs:21013` and the composite
  `NewWorktree { name, … }` arm at `run.rs:18952` both route through
  `new_worktree_target` with the three-arm `Root`/`Refuse`/
  `ActiveFallback` mapping. **OK (modulo the P1 above)**.
- **§5 (regression tests)** — issue-named tests present and pass:
  `cursor_repo_root_is_none_for_a_live_fallback_workspace_row` (locks
  the refusal precondition), `new_worktree_key_refuses_a_live_fallback_workspace_row`
  (key path), `new_worktree_target_maps_focus_rows_and_fallbacks`
  (helper mapping), and the four `heal_*` tests + the
  end-to-end `healed_live_fallback_renders_its_registered_worktrees`
  (adjusted to `unhealed_wt_count < healed_wt_count` because the
  live session group is always rendered, per chunk-2 done-doc — the
  spirit of the test is preserved: registry rows render only after
  the heal, which is the user-visible fix). **OK**.
- **§6 (chunking discipline)** — chunks 1/2/3 are file-disjoint and
  all three lane done-docs match `git diff --stat`. Chunk 3's
  "one third copy" claim is now refuted by the P1 finding.
- **Ratchet impact** — no new action ids, no new ignored `Result`s,
  no new `let _ = …` patterns introduced; help ratchet and
  ignored-result ratchet are untouched. No new help-page prose
  required. **OK**.

## Unverified items (per chunk done-docs)

| Chunk | Unverified item                                               | Architect verdict                                                                                                             |
| ----- | ------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| 1     | `ring_snapshot()` warn-once assertion left as review-verified | acceptable — the per-process `Once` is provably correct by Rust semantics; the ring assertion was optional per the chunk spec |
| 2     | No `just test` / `just ci` / e2e                              | acceptable per dev-loop policy and chunk spec; the targeted `cargo nextest` filters are the chunk's gate and all pass         |
| 3     | No `just test` / `just ci` / e2e                              | acceptable per dev-loop policy; the `new_worktree` filter is the chunk's gate and passes                                      |

## Decision

**REVISE.** Apply the single-arm rewrite in
`.thegn/pipeline/THE-87/architect-review/chunk-3-revise.md` (commit
subject `fix(sidebar): refuse cross-workspace on new-worktree-from-template`),
then this is APPROVED. Re-run
`cargo nextest run -p thegn-host -E 'test(new_worktree) | test(cursor_repo_root)'`
to confirm green, and widen the done-criteria grep to
`rg -n "sidebar_workspaces\.iter\(\)\.find|sidebar_workspaces\.iter\(\)\.position|sidebar_workspaces\.iter\(\)\.filter" crates/thegn-host/src/run.rs`
(which must return nothing) to lock the invariant forward.
