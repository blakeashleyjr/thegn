# Sidebar: one Pipelines section, one-row lanes, finished pipelines age out

## Why

At about 1,700 worktrees the workspace tree showed two groups called
`Pipelines` (a user folder and the derived group), two rows per lane that both
repeated a raw `local:` identity, integration and release branches filed as
pipelines, merged rows that showed a truncated commit subject, and finished
pipelines that never left the list.

## What Changes

- A user folder named `Pipelines` no longer overrides pipeline filing: a
  roster-owned worktree filed there joins the one derived `Pipelines` group.
- Lane names drop the `local:` provider prefix; a single-worktree lane is one
  row named for the lane.
- Integration and release worktrees (`tg/bNNN-integration`, `-release`) stay
  with ordinary worktrees.
- Rows filed under `Merged` show age and size, not the last commit subject.
- Finished lanes fold into a `Finished (N)` sub-group, leave the sidebar after
  `[ui] sidebar_finished_pipeline_days` (default 7, `0` = never), and can be
  hidden at once with the `Finished` row's **Clear finished** menu action.
  Worktrees are never deleted.

## Impact

- **Specs:** `sidebar` — pipeline grouping and lane rows.
- **Code:** `sidebar.rs`, `sidebar_pipeline.rs`, `sidebar_detail.rs`, sidebar
  menu and persisted view state; `[ui] sidebar_finished_pipeline_days`.
