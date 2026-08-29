# THE-87 · Chunk 3 — `thegn-host`: never silently cross workspaces — refuse new-worktree on an unresolvable row

Issue: https://linear.app/blakeashley/issue/THE-87 · Design: `.thegn/pipeline/THE-87/architect/design.md` §4 · HEAD `a65b42a3` (citations against this HEAD)

**Crate:** `thegn-host`. **Parallelizable:** yes — file-disjoint from chunk 1
(`thegn-core`) and chunk 2 (`hydrate.rs` + `hydrate_tests.rs` +
`handlers/switch.rs`); no logical dependency on either. In particular: do NOT
touch `hydrate.rs`, `hydrate_tests.rs`, or `handlers/switch.rs` (chunk 2 owns
them).

## Files touched (exact paths)

| Path                                             | Change                                                                                                                                                                                                                                                                                                                                        |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-host/src/handlers/sidebar_keys.rs` | NEW `NewWorktreeTarget` enum + `SidebarState::new_worktree_target` + `SidebarState::new_worktree_outcome` beside `cursor_repo_root` (`:150-175`); rewrite the `Id::NewWorktree` arm (`:739-748`) and the menu `"new-worktree"` arm (`:1186-1191`) to use them; refusal tests beside `cursor_repo_root_uses_hydrated_workspace_path` (`:1271`) |
| `crates/thegn-host/src/run.rs`                   | Replace the inline sidebar lookup + active-group fallback in `Action::NewWorktree` (`:20992-21031`) and in composite `NewWorktree { … }` (`:18952-18990`) with `sb.new_worktree_target(...)` — minimal arm rewrites, NO new logic invented here                                                                                               |
| `crates/thegn-host/src/run_tests.rs`             | Optional: helper-level test for the target mapping if the coder prefers it there instead of `sidebar_keys.rs` — chunk 3 is the only chunk allowed to touch this file                                                                                                                                                                          |

Nothing else. No keymap/spec change, no help-page change (no new action ids —
`Id::NewWorktree` / `Action::NewWorktree` exist; the help ratchet is
unaffected), no config keys, no e2e snapshot change expected.

## Approach

1. **The helper** (in `sidebar_keys.rs`, reusing `cursor_repo_root` — no
   duplicated slug logic):
   ```rust
   /// What `Action::NewWorktree` should do, given the sidebar cursor.
   pub(crate) enum NewWorktreeTarget {
       /// The cursor row's repo resolved — open the wizard there.
       Root(String),
       /// The cursor row names a workspace/worktree/folder but its repo is
       /// unresolvable (a live fallback whose registry heal also failed).
       /// Refuse — NEVER silently build in another workspace's repo.
       Refuse(&'static str),
       /// No sidebar row in play (no sidebar focus, no selected row, or a
       /// terminals-region row): the active tab's repo is the intent, as before.
       ActiveFallback,
   }
   ```
   `pub(crate) fn new_worktree_target(&self, model: &FrameModel, sidebar_focus: bool) -> NewWorktreeTarget`, mapping in order:
   - `!sidebar_focus` or `self.selected_row(model).is_none()` ⇒ `ActiveFallback`
     (preserves today's Alt+w/palette behaviour — creating in the active tab's
     repo is the intended semantic when no row is selected);
   - `self.cursor_in_terminals(model)` ⇒ `ActiveFallback` (defensive: both
     existing pre-guards — sidebar `n` at `:739-741` and the run.rs guard arm
     at `:20980-20991` — already convert terminals rows to NewTerminal);
   - otherwise `self.cursor_repo_root(model)`: `Some(root)` ⇒ `Root(root)`,
     `None` ⇒ `Refuse(NEW_WORKTREE_REFUSAL)`.
     `const NEW_WORKTREE_REFUSAL: &str = "No repo path for this workspace yet — it registers on the next refresh";`
     (wording may be tuned; it must name the cause and the remedy and must NOT
     imply the worktree was created anywhere).
2. **Extract the arm body**: `fn new_worktree_outcome(&self, model: &mut
FrameModel) -> SidebarOutcome` used by BOTH the `Id::NewWorktree` key arm
   (`:739-748`) and the menu `"new-worktree"` arm (`:1186-1191`), so tests can
   drive it without keymap chord resolution:
   - `cursor_in_terminals` ⇒ `SidebarOutcome::Synthetic(Action::NewTerminal)`
     (unchanged);
   - `cursor_repo_root` resolves ⇒ `SidebarOutcome::NewWorktreeIn { repo_root }`
     (unchanged);
   - otherwise ⇒ **refuse**: `model.status = NEW_WORKTREE_REFUSAL.into();` and
     return `SidebarOutcome::Redraw` after `self.sync(model)` (mirror
     `Id::Delete`'s Essential-tier "never silently no-op" pattern at `:727-736`).
     Today the `None ⇒ Synthetic(Action::NewWorktree)` fall-through's ONLY
     downstream effect was the cross-workspace active-group fallback at
     `run.rs:21012-21016` — refusing strictly narrows to the safe case.
3. **`run.rs:20992-21031` (`Action::NewWorktree`)** — replace the inline
   `sidebar_repo` lookup (`.filter(|p| !p.is_empty())` at `:21005`) and the
   `unwrap_or_else(active_group)` at `:21012-21016` with:
   - `Root(root)` ⇒ keep the existing `main_worktree` normalization
     (`:21017-21022`; a `git rev-parse` — pre-existing, user-initiated,
     sanctioned post-frame) + `begin_worktree_wizard`;
   - `Refuse(msg)` ⇒ `model.status = msg.into(); dirty = true;`;
   - `ActiveFallback` ⇒ keep today's `session.active_group()` →
     `main_worktree` → `current_dir` ladder and the
     `thegn_core::msg::warn("new-worktree: not inside a git repository")` tail
     (`:21023-21030`).
4. **`run.rs:18952-18990` (composite `NewWorktree { name, sandbox, agent,
base }`)** — the same bug shape ("Same repo-root resolution as
   Action::NewWorktree", fallback at `:18972-18976`): replace the duplicated
   lookup with `sb.new_worktree_target(&model, focus.sidebar())` — `Root` ⇒
   the same `main_worktree` normalization + `begin_worktree_preset`; `Refuse`
   ⇒ status + `dirty`; `ActiveFallback` ⇒ today's ladder. One helper closes
   both holes; no third copy of the resolution may remain
   (`grep -n "sidebar_repo" crates/thegn-host/src/run.rs` must return only the
   two rewritten arms or nothing).

## Tests (scoped)

```
just quick thegn-host
cargo nextest run -p thegn-host cursor_repo_root
cargo nextest run -p thegn-host new_worktree
```

New tests beside `cursor_repo_root_uses_hydrated_workspace_path`
(`sidebar_keys.rs:1271` — reuse its `FrameModel`/`SidebarRow` construction
pattern):

- `cursor_repo_root_is_none_for_a_live_fallback_workspace_row` (issue-mandated
  name) — a `Workspace` row whose `sidebar_workspaces` entry has an empty
  repo path resolves to `None` (the refusal precondition, pinned).
- `new_worktree_key_refuses_a_live_fallback_workspace_row` —
  `new_worktree_outcome` on such a row sets a non-empty `model.status` and
  returns `SidebarOutcome::Redraw`; it must NOT return
  `SidebarOutcome::Synthetic(crate::keymap::Action::NewWorktree)`.
- `new_worktree_target_maps_focus_rows_and_fallbacks` —
  `sidebar_focus = false` ⇒ `ActiveFallback`; focused resolvable workspace
  row ⇒ `Root`; focused unresolvable repo-ish row ⇒ `Refuse`; terminals row ⇒
  `ActiveFallback` (defensive arm).

Existing tests that must keep passing unmodified:
`cursor_repo_root_uses_hydrated_workspace_path` (`:1271`),
`fork_outcome_uses_hydrated_workspace_path` (`:1301`), and the run.rs-side
tests around `NewWorktree` if any reference the old inline lookup
(`grep -n "NewWorktree" crates/thegn-host/src/run_tests.rs` — update only if
they assert the old fallback-from-unresolvable-row behaviour, which would be
a regression now).

## Done-criteria

- `just quick thegn-host` clean; scoped nextest filters above green.
- The three issue-named behaviours hold: refusal on unresolvable sidebar rows
  (key + menu + palette/global action), active-tab fallback preserved when no
  sidebar row is in play, and `cursor_repo_root`'s `None` pinned by test.
- Exactly ONE resolution implementation remains: no duplicated
  slug→`sidebar_workspaces` lookup inline in `run.rs`.
- No new action ids / chords (help ratchet untouched); no new ignored
  `Result`s (ignored-result ratchet untouched); no e2e-relevant frame change
  (a status line only) — do NOT re-record snapshots.
- `git diff --stat` shows ONLY the files listed above (run.rs changes are the
  two arm rewrites, nothing else).

**Commit subject (exact):** `fix(sidebar): never cross workspaces on new-worktree; refuse unresolvable rows`
