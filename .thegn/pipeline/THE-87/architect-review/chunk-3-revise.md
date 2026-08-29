# THE-87 · Chunk 3 — Revision: close the third NewWorktree cross-workspace site

Issue: https://linear.app/blakeashley/issue/THE-87 · Design: `.thegn/pipeline/THE-87/architect/design.md` §4 · Branch `tg/the-87-live-fallback-workspace`

**Crate:** `thegn-host`. **Scope:** one file, one arm.

## What's missing

The architecture review found a **third** call site that resolves
"sidebar-selected workspace repo path, else active group's path" — the
exact same wrong-workspace shape the design's "one helper closes both
holes; no third copy of the resolution may remain" was meant to lock
closed. It slipped because the chunk-3 done-criteria verification used
a too-narrow regex (`grep -n "sidebar_repo" crates/thegn-host/src/run.rs`)
that wouldn't catch this code path — it walks `model.sidebar_workspaces`
via `.iter().find()`, not via the `sidebar_repo` token.

The site is `Action::NewWorktreeFromTemplate` at
`crates/thegn-host/src/run.rs:20457`, and the comment immediately above
its resolution is honest about what it's doing:

```rust
// Resolve the repo root the same way NewWorktree does:
// the sidebar-selected workspace, else the active group.
```

The bug shape, traced against `live-fallback` rows:

1. User clicks a workspace whose `repo_path` is empty (a live fallback
   whose registry read failed — exactly the residual window §1+§3
   acknowledge).
2. `selected_row(model).workspace_slug` resolves to that slug; the inline
   `iter().find()` returns `Some(("", "", "repo", ""))`.
3. `.filter(|p| !p.is_empty())` drops it.
4. `.unwrap_or_else(|| active_group().map(|g| g.path))` falls through to
   the _active tab's_ path.
5. `main_worktree(active)` is taken; the worktree is created in the
   _active tab's_ repo, not the workspace the user clicked.

Identical to the bug the design targets — and identical to the two
sites chunk 3 closed. The chunk spec's "no third copy of the resolution
may remain" invariant was breached.

## Fix

Route the template action through the same
`sb.new_worktree_target(&model, focus.sidebar())` helper the other two
sites now use. Three arms, exactly mirroring `Action::NewWorktree`:

- `Root(root)` → resolve to `main_worktree`, open
  `HostInputKind::NewWorktreeFromTemplate { repo_root }` (today's happy
  path).
- `Refuse(msg)` → `model.status = msg.into(); dirty = true; continue;`
  (today the action would have silently cross-workspace-built — refuse
  is the right behaviour here, same as `n`).
- `ActiveFallback` → today's `active_group().path` → `main_worktree`
  ladder. The template action has no `current_dir` fallback today
  (it never did), so the
  `or_else(|| std::env::current_dir().ok().and_then(main_worktree))`
  tail added to `Action::NewWorktree` does NOT belong here — keep the
  template's existing behaviour, which is "active group only, else
  warn". This is intentional: templates are an explicit user gesture
  against the active workspace, and silently catching the `current_dir`
  is not what the template picker wants.

## Files touched (exact paths)

| Path                           | Change                                                                                                                                                                                                                                                                                                                      |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-host/src/run.rs` | Replace the inline `selected_row → workspace_slug → sidebar_workspaces.find → main_worktree` block in `Action::NewWorktreeFromTemplate` with `sb.new_worktree_target(&model, focus.sidebar())` (3 arms, no third resolution copy). Do NOT add a `current_dir` fallback to the `ActiveFallback` arm (intentional narrowing). |

Nothing else. `handlers/sidebar_keys.rs`, `hydrate.rs`, `handlers/switch.rs`,
`db.rs`, `db_migrate.rs`, and the test files for chunks 1/2/3 stay
untouched.

## Approach (concrete diff sketch, not a patch)

The current 35-line block at `run.rs:20464-20498` becomes:

```rust
Action::NewWorktreeFromTemplate => {
    let names: Vec<String> =
        crate::layout_spec::worktree_templates_with_imports(&current_config)
            .iter().map(|t| t.name.clone())
            .filter(|n| !n.is_empty()).collect();
    // Single resolution via the sidebar_keys helper (never silently crosses
    // workspaces — same shape as Action::NewWorktree / composite NewWorktree).
    // The template action intentionally has NO current_dir fallback: the
    // template picker is an explicit gesture against the active workspace,
    // not a CD-relative convenience.
    match sb.new_worktree_target(&model, focus.sidebar()) {
        NewWorktreeTarget::Root(root) => {
            let repo_root = thegn_core::repo::main_worktree(Path::new(&root))
                .map(|p| p.to_string_lossy().into_owned());
            match (names.is_empty(), repo_root) {
                (true, _) => model.status = "No [[worktree_templates]] configured".into(),
                (false, None) => model.status = "New worktree: not inside a git repository".into(),
                (false, Some(root)) => {
                    host_input = Some((
                        menu::InputOverlay::new("worktree template (name)", ""),
                        HostInputKind::NewWorktreeFromTemplate { repo_root: root },
                    ));
                    model.status = format!("New worktree from template — {}", names.join(", "));
                }
            }
        }
        NewWorktreeTarget::Refuse(msg) => {
            model.status = msg.into();
            dirty = true;
            continue;
        }
        NewWorktreeTarget::ActiveFallback => {
            let src_wt = session.active_group().map(|g| g.path.clone()).unwrap_or_default();
            let repo_root = (!src_wt.is_empty())
                .then(|| thegn_core::repo::main_worktree(Path::new(&src_wt)))
                .flatten()
                .map(|p| p.to_string_lossy().into_owned());
            match (names.is_empty(), repo_root) {
                (true, _) => model.status = "No [[worktree_templates]] configured".into(),
                (false, None) => model.status = "New worktree: not inside a git repository".into(),
                (false, Some(root)) => {
                    host_input = Some((
                        menu::InputOverlay::new("worktree template (name)", ""),
                        HostInputKind::NewWorktreeFromTemplate { repo_root: root },
                    ));
                    model.status = format!("New worktree from template — {}", names.join(", "));
                }
            }
        }
    }
}
```

Net delta: +18 lines, -16 lines, no behavioural change for the
`ActiveFallback` arm (kept identical, no `current_dir` fallback added),
and the `Refuse` arm added to close the cross-workspace hole on a
focused live-fallback row.

## Tests (scoped)

```
just quick thegn-host
cargo nextest run -p thegn-host new_worktree
```

No new test file. The existing
`new_worktree_target_maps_focus_rows_and_fallbacks` test in
`handlers/sidebar_keys.rs` (which drives `new_worktree_target` directly)
already locks the three-arm mapping; the template action is now a
consumer of that mapping, and the `Refuse` arm behaviour is identical
to the `Refuse` arm on `Action::NewWorktree` (which the
`new_worktree_key_refuses_a_live_fallback_workspace_row` test already
pins at the helper level).

If the coder wants a belt-and-suspenders run.rs-level test (the chunk
spec said `run_tests.rs` is the optional home for helper-level tests),
add a `new_worktree_from_template_refuses_a_live_fallback_workspace_row`
that mirrors the `new_worktree_key_refuses_*` test but feeds a
fake-host-input capture instead of asserting on `model.status` — but
this is recommended, not required: the helper is unit-tested and the
template action is now a thin pass-through to it.

## Done-criteria

- `just quick thegn-host` clean; `cargo nextest run -p thegn-host
new_worktree` green.
- The 3-way grep "no third copy" now actually holds. The verification
  pattern must be widened — replace the chunk-3 grep:
  ```
  rg -n "sidebar_workspaces\.iter\(\)\.find|sidebar_workspaces\.iter\(\)\.position|sidebar_workspaces\.iter\(\)\.filter" crates/thegn-host/src/run.rs
  ```
  must return nothing (today it returns 1: the template site).
- `Action::NewWorktreeFromTemplate` no longer has an inline
  `model.sidebar_workspaces.iter().find(...)` block.
- The `ActiveFallback` arm keeps the template action's pre-existing
  behaviour exactly: `active_group().path` only, no `current_dir`
  fallback. (This is a deliberate narrowing vs. `Action::NewWorktree`,
  which got a `current_dir` fallback added in chunk 3.)
- The `Refuse` arm is byte-identical to the `Refuse` arm in
  `Action::NewWorktree` (same refusal message, same `model.status =
msg.into(); dirty = true; continue;` shape).
- `git diff --stat` shows ONLY the single `crates/thegn-host/src/run.rs`
  file in this commit. `run.rs` changes are EXACTLY the one arm
  rewrite, nothing else.

**Commit subject (exact):**
`fix(sidebar): refuse cross-workspace on new-worktree-from-template`

## Out of scope (explicitly)

- Adding a `current_dir` fallback to the template's `ActiveFallback`
  arm. The template picker is a "use my active workspace" gesture;
  catching the shell CWD is what `Action::NewWorktree` does and is the
  right semantic for an unsourced Alt+w, not for an explicit
  template-pick. If a future UX round wants them aligned, that is a
  separate change.
- The other 7 `Action::NewWorktree` textual references in `run.rs`
  (palette-suggestion push at 15341/15426/16828 etc.) — those are
  `forced_palette_action` enqueues, not resolution sites, and don't
  reach the bug shape.
- Any other "resolve a repo from a workspace slug" sites in
  `handlers/`. A `rg "model\.sidebar_workspaces\.iter" crates/thegn-host/src`
  sweep confirms there are none — only the three sites this branch
  touched.
