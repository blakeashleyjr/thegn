# THE-87 · Chunk 3 completion

Implemented the `thegn-host` guardrail so new-worktree never silently crosses into another workspace when the sidebar row has no resolvable repository path.

## Changes

- Added `NewWorktreeTarget` and the shared `SidebarState::new_worktree_target` mapping:
  - focused resolvable rows target their repository;
  - focused unresolvable workspace/worktree/folder rows refuse with an actionable status message;
  - no sidebar row, no sidebar focus, and terminal-region rows preserve active-tab fallback semantics.
- Added `SidebarState::new_worktree_outcome` and routed both the sidebar key and context-menu `new-worktree` paths through it.
- Replaced both duplicated `run.rs` sidebar repository lookups (global `Action::NewWorktree` and composite new-worktree actions) with the shared target helper.
- Added the three requested regression tests for live-fallback refusal, the unresolved repo-root precondition, and target mapping.

## Verification

- `just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host cursor_repo_root` — passed (2 tests).
- `cargo nextest run -p thegn-host new_worktree` — passed (5 tests).
- `git diff --check` — passed.
- No `sidebar_repo` lookup remains in `crates/thegn-host/src/run.rs`.

## Unverified

- Full-workspace gates (`just test`, `just ci`, and `just coverage`) were not run, per the chunk instructions and dev-loop policy.
- E2E tests were not run, per the chunk instructions.
- Workspace-wide `cargo fmt --all -- --check` still reports pre-existing formatting differences in unrelated `gtui-app`/`thegn-svc` files; the changed `thegn-host` files have no formatting diff reported.
