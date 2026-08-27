# THE-73 · chunk 1 — sidebar renders the union of live groups and registry rows

**Read first:** `.thegn/pipeline/THE-73/architect/design.md` §1 and §2/F1.
`CLAUDE.md` (dev-loop policy, ratchets) is binding.

## Files touched (exact)

- `crates/thegn-host/src/sidebar.rs` — `gather_groups` + its inline `mod tests`
  (module starts at line 1667).

**Nothing else.** Do not touch `hydrate.rs`, `session.rs` or `run.rs` — chunks 2
and 3 own those.

## Overlap / dependency

None. File-disjoint from chunk 2 (`hydrate.rs`, `hydrate_tests.rs`) and chunk 3
(`session.rs`, `run.rs`). **Runs in parallel with both.**

## The defect

`crates/thegn-host/src/sidebar.rs:1173-1258`, `gather_groups`:

```rust
for (gi, g) in session.worktrees.iter().enumerate() { …live Groups… }  // 1182-1202
let live = !groups.is_empty();                                        // 1203
if !live && !repo_path.is_empty() {                                   // 1210
    …synthesize Groups from db_by_slug…                               // 1211-1255
}
```

The DB-backed fill is **all-or-nothing**: one live session group for a workspace
suppresses _every_ registry row of that workspace. So a real, registered,
git-listed worktree that the session did not adopt is invisible while the
workspace is live and visible while it is dormant — which is exactly the
reported "click the sidebar → the row vanishes; switch workspaces → it comes
back" symptom (design §1).

## Approach

Make the DB fill **additive**, not exclusive.

1. Delete the `let live = !groups.is_empty();` gate (line 1203) and the `!live &&`
   conjunct on line 1210. Keep `!repo_path.is_empty()` — a _live-fallback_
   workspace entry carries an empty `repo_path` (`hydrate.rs:1285-1293`) and has
   no switch target, so it must still contribute no synthetic rows.

2. Before the fill, build the set of tab names and the set of paths already
   covered by live groups. A live `Group` does not carry its tab name, so
   collect it in the live loop (you already have `g.name` there) into a local
   `Vec`/`HashSet` alongside `groups`; also collect `g.path`.

3. Emit the synthetic `home` Group **only when no live group covers
   `{repo_slug}/home`** (today it is emitted unconditionally inside the `!live`
   branch). Same for each non-home registry row: skip it when its `tab_name` is
   already covered, or when its `path` is already covered by a live group (a
   group renamed in-session keeps the same path — matching on path second stops
   a duplicate row).

4. Keep everything else about the synthesized `Group`s byte-identical to today:
   `label`, `sandbox_backend`, `env_name`, `env_degraded`, `folder_id`,
   `activity` keyed by tab name, `active: false`, and
   `target: RowTarget::Workspace { repo_path, group: Some(tab_name) }`
   (lines 1216-1255). Do **not** invent a new `RowTarget` variant.

5. `gi` on synthesized groups is a sort tie-break only
   (`Group.gi` doc, lines 588-591). Today the dormant branch numbers them
   `0` for home and `i + 1` for the rest. When live groups are present they
   already occupy `gi` values that are real session indices, so number the
   appended ones **after** the highest live `gi` (e.g. `live_max + 1 + i`) so the
   tie-break stays stable and appended rows sort after their live siblings under
   `SortMode::Manual`. Note this in a comment.

6. Update `gather_groups`' doc comment (lines 1164-1172): it currently says a
   loaded workspace draws groups "straight from the session model" and a dormant
   one is "reconstructed from the DB". It must now say: live groups first, then
   every registered worktree the session does not already carry — git/the
   registry is the source of truth, so a registered worktree is never invisible
   just because the session missed it.

Both emitters (`build_rows` at `sidebar.rs:877` and `build_rows_flat` at
`sidebar.rs:1355`) call this one function, so both are fixed at once.

### Guardrails

- The function must stay **pure** — no DB, no git, no filesystem, no `Instant`.
- No colour or glyph literal, no `#[cfg]`, no ignored `Result`. Do not add an
  entry to any `test/*-ratchet.txt`.
- Strict widening: every row that renders today must still render, in the same
  order, with the same `pin_key`. The existing dormant-workspace tests
  (`sidebar.rs:2962`, `:3017`, `:3043`, `:3073`) and every other test in the
  module must pass **unchanged** — if one needs editing, you have changed
  behaviour that was pinned; stop and say so in your report instead of
  rewriting the assertion.

## Tests to add

In `crates/thegn-host/src/sidebar.rs`'s `mod tests`, beside
`dormant_workspace_renders_same_structure_as_live` (line 3017):

1. `registered_worktree_renders_even_when_the_session_missed_it` — a session
   holding only `{slug}/home` for a live workspace, plus a `DbWorktree` for
   `{slug}/foo` whose `path` is **far outside any `worktrees_dir`** (use
   something like `/home/other-profile/.elsewhere/wt/foo` and say so in a
   comment — this is the THE-73 case). Assert a `RowKind::Worktree` row for
   `foo` exists, at depth 1, with
   `RowTarget::Workspace { repo_path, group: Some("{slug}/foo") }`.

2. `a_live_group_is_not_duplicated_by_its_registry_row` — the same
   `{slug}/foo` present _both_ as a live session group and as a `DbWorktree`.
   Assert exactly one row for it and that its target is `RowTarget::Tab(..)`
   (the live one wins).

3. `a_live_fallback_workspace_still_synthesizes_nothing` — workspace entry with
   an empty `repo_path` (the `hydrate.rs:1285-1293` shape) plus a matching
   `DbWorktree`; assert no synthetic row is emitted (unchanged behaviour).

4. Extend or add a flat-mode assertion so `build_rows_flat` is covered by the
   same union rule (there are existing flat tests in the module — mirror the
   nearest one).

## Commands to run (scoped only)

```sh
just quick thegn-host
cargo nextest run -p thegn-host sidebar
```

**Do not run** `just test`, `just ci`, `just coverage`, `just lint`, `just e2e`,
or any full-workspace compile. The Lead owns the pre-push gate.

## Done criteria

- `gather_groups` emits live groups **plus** the uncovered registry rows; the
  `!live` gate is gone; the doc comment reflects the new contract.
- The four new tests pass; every pre-existing test in `sidebar.rs`'s `mod tests`
  passes **unedited**.
- `just quick thegn-host` is clean (clippy `-D warnings`).
- No ratchet file modified.
- Committed with exactly this subject:

  ```
  fix(sidebar): render registered worktrees the session missed (THE-73)
  ```
