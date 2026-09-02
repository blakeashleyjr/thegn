# Tasks — nest sidebar pipelines

## 1. Attribution (thegn-host)

- [x] 1.1 Build a `dir_owner` map (worktree parent directory → project slug,
      ambiguous entries dropped) from the same sources `lane_targets` walks.
- [x] 1.2 Consult it only when the direct `lane_targets` lookup fails, so the
      mirror leaves keep their resolved `RowTarget`.
- [x] 1.3 A lane with no home contributes no rows.

## 2. Remove the depth-0 rows (thegn-host)

- [x] 2.1 Delete the `pipeline/group:unfiled` tail call; flat layout therefore
      emits no pipeline rows.
- [x] 2.2 Delete the `PipelineSummary` emission, its `RowKind` variant, the
      `SidebarRow::pipeline` field, both paint arms, and the synthetic
      board-open paths in the mouse and key handlers.
- [x] 2.3 Delete `SidebarStatus::pipeline`, its write in `attention_status.rs`,
      and the now-unreachable `monitor_pipeline::summary` fold and its tests.

## 3. Tests

- [x] 3.1 An unregistered sibling worktree files under its project.
- [x] 3.2 A lane no project claims emits nothing.
- [x] 3.3 The flat layout emits no pipeline rows.
- [x] 3.4 Nothing pipeline-shaped renders at depth 0.

## 4. Docs

- [x] 4.1 `docs/help/sidebar.md`: drop the rollup row, state the nesting rule
      and where an unattributable lane goes instead.

## 5. Validation

- [ ] 5.1 `just ci` (pre-PR gate, run once).
