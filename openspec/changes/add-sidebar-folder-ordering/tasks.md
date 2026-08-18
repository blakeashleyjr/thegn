# Tasks

## 1. Ordering model

- [x] 1.1 New pure module `crates/thegn-host/src/sidebar_order.rs`: `runs()`
      partitions a workspace into `(folder_id)`-keyed sibling runs from the
      rendered row tree (depth-2 worktrees belong to the header above them),
      loose first then folders in header order.
- [x] 1.2 `step()` — one slot within a run, crossing into the adjacent
      enterable run at an edge (collapsed folders skipped), `home` anchored.
      Returns a `Plan { path, order, refile }` carrying the workspace's whole
      new order.
- [x] 1.3 `drop_at()` — land before an anchor (or at a run's end) for the
      mouse; refuses landing above `home` and refuses a vanished anchor.
- [x] 1.4 `folder_order()` / `step_folder()` / `drop_folder_at()` and the
      `in_run_neighbor()` / `next_in_run()` / `run_of()` helpers.
- [x] 1.5 16 unit tests covering run partitioning, both crossing directions,
      collapsed-folder hop, home anchoring, outer edges, and folder moves.

## 2. Store

- [x] 2.1 `set_worktree_order(&[String])` on `WorkspaceStore` + `Db`:
      `position = index` in one transaction (mirrors `set_workspace_order`).
- [x] 2.2 `set_folder_order(repo_path, &[i64])`: the first mutator for
      `folders.position`, scoped by `repo_path` so a foreign id can't
      renumber another workspace.
- [x] 2.3 Core unit tests for both, including the NULL/tied-position case
      and the cross-workspace guard. No schema change, no version bump.

## 3. Handlers

- [x] 3.1 `apply_order_plan`: optimistic re-file + model re-sort (which is
      what makes dormant workspaces reorder), live session permutation with
      the active group tracked by name, sort→Manual flip, off-loop persist.
- [x] 3.2 `move_worktree_path` / `move_cursor_worktree` / `move_folder_id` /
      `apply_folder_order`; `move_worktree_group` and its swap+step machinery
      deleted.
- [x] 3.3 `persist_worktree_order` / `persist_folder_order` run on
      `spawn_blocking` (inline when there is no runtime, for tests) — the old
      path opened the DB on the event loop.
- [x] 3.4 `reorder_selection`: worktree arm re-keyed on paths and routed
      through the run model; new `RowKind::Folder` arm.
- [x] 3.5 `run.rs` `MoveItem` dispatch: `Folder` arm, and `Worktree` moves
      the cursor row instead of falling through to the active worktree.

## 4. Mouse

- [x] 4.1 `Spot::Reorder` carries the destination run; `spot_at` resolves it
      from the hovered row and stops the bottom-half search at the run
      boundary.
- [x] 4.2 `DragSrc::Folder` + folder spot resolution (header top-half =
      before, subtree = after, workspace header = first); synthetic negative
      folder ids are not draggable.
- [x] 4.3 `perform_drop` routes through `drop_at` / `drop_folder_at` — the
      bounded step-swap loop and `worktree_run` are gone; header file/unfile
      drops also land at the end of the new run.
- [x] 4.4 Unit tests for folder-targeted drops, run-boundary clamping,
      folder drags, and the synthetic-id guard.

## 5. Docs

- [x] 5.1 `docs/help/sidebar.md`: a "Reorder" section (runs, crossing,
      folder ordering, sort→manual) and a "Mouse" section — the sidebar's
      mouse gestures were entirely undocumented despite being implemented.
      No new action ids, so the help ratchet is unaffected.
- [ ] 5.2 Live TUI pass: reorder inside a folder, cross both edges, hop a
      collapsed folder, drag between rows inside a folder, drag a folder
      header, flat-mode toggle, restart persistence.
- [ ] 5.3 `just ci` before the PR.
