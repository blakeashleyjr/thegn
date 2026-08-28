# THE-73 · chunk 1 — done

**Commit:** `bb05bdcb` — `fix(sidebar): render registered worktrees the session missed (THE-73)`
**Files touched:** `crates/thegn-host/src/sidebar.rs` only (impl + its inline `mod tests`).
No other file, no ratchet file, no `hydrate.rs` / `session.rs` / `run.rs`.

## What changed

`gather_groups` now emits the **union** of the session's live groups and the DB
registry, instead of choosing one source per workspace.

1. **`let live = !groups.is_empty();` deleted; the `!live &&` conjunct dropped.**
   `!repo_path.is_empty()` kept verbatim — a live-fallback workspace entry
   carries an empty `repo_path` and has no switch target, so it still
   synthesizes nothing.
2. **Coverage keys collected in the live loop.** Two `HashSet<&str>` —
   `live_tabs` (from `g.name`, which the `Group` itself does not retain) and
   `live_paths` (from `g.path`). Empty paths are never inserted and never
   consulted, so pathless rows can't collide with each other.
3. **`home` is emitted only when no live group covers `{repo_slug}/home`.**
   Each non-home registry row is skipped when its `tab_name` is covered, or
   (second key) when its non-empty `path` is covered — that path key is what
   stops a group renamed in-session from rendering twice.
4. **Synthesized `Group`s are otherwise unchanged**: same `label`,
   `sandbox_backend`, `env_name`, `env_degraded`, `folder_id`, `activity` keyed
   by tab name, `active: false`, and
   `RowTarget::Workspace { repo_path, group: Some(tab_name) }`. No new
   `RowTarget` variant.
5. **`gi` numbering.** A running `next_gi` starts at `max(live gi) + 1`, or `0`
   when there are no live groups. With no live groups that reproduces the old
   dormant numbering byte-for-byte (`home` = 0, then 1, 2, …); with live groups
   the appended rows sort after their live siblings under `SortMode::Manual`.
   Commented in place.
6. **Doc comment rewritten** to state the union contract and why (git/the
   registry is the source of truth; an adoption miss degrades to "the row
   switches the workspace instead of focusing a live tab", not "the row
   disappeared").

Purity held: no DB, git, filesystem or `Instant`; no colour/glyph literal, no
`#[cfg]`, no ignored `Result`. Both emitters (`build_rows`, `build_rows_flat`)
call this one function, so both are fixed.

## Tests added (5, all in `sidebar.rs`'s `mod tests`)

A shared `foreign_dir_registry_row()` fixture supplies the THE-73 shape: an
`app` workspace at `/repos/app` plus a `DbWorktree` for `app/foo` whose path is
`/home/other-profile/.elsewhere/wt/foo` — far outside any `worktrees_dir`,
commented as such.

- `registered_worktree_renders_even_when_the_session_missed_it` — session holds
  only `app/home`; asserts the `foo` `RowKind::Worktree` row exists at depth 1
  with `RowTarget::Workspace { repo_path: "/repos/app", group: Some("app/foo") }`,
  and that the live `home` sibling is untouched.
- `a_live_group_is_not_duplicated_by_its_registry_row` — `app/foo` present both
  live and in the registry; asserts exactly one row and that its target is
  `RowTarget::Tab(..)` (the live one wins).
- `a_live_group_renamed_in_session_is_not_duplicated_by_path` — the path key's
  own guard: a live group renamed to `app/renamed` still sharing `/wt/foo`
  yields exactly one row for that path. (Beyond the spec's list; the path
  fallback would otherwise be untested.)
- `a_live_fallback_workspace_still_synthesizes_nothing` — workspace entry with
  an empty `repo_path` plus a matching `DbWorktree`; asserts no synthetic row.
- `flat_layout_renders_registered_worktrees_the_session_missed` — the flat-mode
  mirror (spec item 4): the missed row renders at depth 1 with
  `repo_prefix == Some("app")`.

## Verification run (scoped only, per the dev-loop policy)

- `cargo nextest run -p thegn-host sidebar` → **209 passed, 0 failed**
  (204 before this chunk, +5 new). **Every pre-existing test passed unedited** —
  no assertion in the module was touched, so nothing that was pinned changed.
- `just quick thegn-host` → clean, no clippy warnings (`-D warnings`).
- `rustfmt --edition 2024` on the single file (not `cargo fmt -p`, to avoid
  touching the sibling chunks' uncommitted files); the pre-commit `treefmt` hook
  then passed on commit.

## Unverified

- **No full-workspace gate was run** (`just test`, `just lint`, `just coverage`,
  `just ci`) — Lead owns the pre-push gate. Only tests whose name matches
  `sidebar` were executed; a test elsewhere in `thegn-host` that renders a
  sidebar tree under a different test-module name would not have been caught.
- **No e2e / snapshot run.** This change can add rows to the sidebar for a live
  workspace whose registry holds worktrees the session missed. In the e2e
  fixtures the session and registry should agree, so no frame should move — but
  that is unverified, and `test/muse/snapshots/` may need a re-record if any
  fixture has a registry row its session lacks.
- **`just quick` does not lint test targets** (lib/bin only), so the new test
  code was clippy-checked only insofar as `cargo nextest` compiled it — it
  compiles warning-free under the test profile, but `clippy --all-targets` was
  not run on it.
- Chunks 2 (`hydrate.rs`) and 3 (`session.rs`, `run.rs`) were in flight
  concurrently in this shared worktree; no interaction was tested. F1 is
  independent by construction (pure, file-disjoint), and it is a strict
  widening, so a chunk-2/3 regression cannot be masked by it — but the combined
  behaviour is unverified here.
