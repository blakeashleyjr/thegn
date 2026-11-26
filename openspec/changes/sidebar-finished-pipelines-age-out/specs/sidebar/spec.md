# Sidebar — finished pipelines age out delta

## ADDED Requirements

### Requirement: One readable Pipelines section whose finished lanes age out

Each project SHALL show at most one group named `Pipelines`. A worktree the
pipeline roster owns that is filed in a user folder named `Pipelines`
(case-insensitive) SHALL render in the derived `Pipelines` group, not in that
folder. Integration and release worktrees (`tg/bNNN-integration`,
`tg/bNNN-release`) SHALL NOT be pipeline rows.

A lane SHALL be labelled with its issue or lane name without a `local:` prefix.
A lane with one worktree SHALL be a single row carrying the lane's name.

A lane whose roster rows are all terminal SHALL render inside one
`Finished (N)` sub-group after the live lanes. It SHALL leave the sidebar once
its last dispatch or exit is older than `[ui] sidebar_finished_pipeline_days`
days (`0` = never), or once the user runs **Clear finished** on that
`Finished` row after it finished. A lane holding the active worktree SHALL
stay. Neither path SHALL delete or move a worktree.

Worktree rows filed under `Merged` SHALL omit the last commit subject.

#### Scenario: A single-worktree lane is one row

- **WHEN** a lane `local:tg-the-9` references one worktree
- **THEN** the sidebar shows one row labelled `tg-the-9` under `Pipelines`

#### Scenario: A finished lane ages out

- **WHEN** every roster row of a lane is terminal and its last activity is
  older than the configured window
- **THEN** the lane is not shown and its worktree still exists

#### Scenario: Clear finished

- **WHEN** the user runs **Clear finished** on a project's `Finished` row
- **THEN** every lane finished by then leaves that project's sidebar, and a
  lane that finishes later appears under `Finished` again
