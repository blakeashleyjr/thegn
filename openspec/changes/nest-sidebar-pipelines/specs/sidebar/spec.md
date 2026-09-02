# Sidebar

## ADDED Requirements

### Requirement: Pipeline rows only ever nest under a project

The sidebar SHALL render derived pipeline folders only inside the project that
owns their worktrees. No pipeline-shaped row — group, lane, worktree mirror, or
roster rollup — SHALL be emitted at the root of the tree.

A lane's project SHALL be resolved from the first of its worktrees that
resolves through the live session groups or the DB-registered worktrees; when
none does, it SHALL be resolved from the directory holding that project's other
worktrees, and a directory claimed by more than one project SHALL be treated as
ambiguous and ignored. A lane that resolves to no project SHALL contribute no
rows to the tree; it remains on the pipeline board, which is the complete view
of the roster.

The flat layout has no project rows to nest under and SHALL therefore emit no
pipeline rows.

The pipeline board SHALL remain reachable independently of any sidebar row, via
`Action::OpenPipelineBoard`.

#### Scenario: A lane nests under the project owning its worktrees

- **WHEN** a lane's worktree resolves to a registered project
- **THEN** its `Pipelines` group, lane folder and worktree mirrors render
  inside that project's subtree, and none of them at depth 0

#### Scenario: An unregistered sibling worktree still finds its project

- **WHEN** a lane's only worktree is not in the session or the database, but
  sits in the directory holding that project's registered worktrees
- **THEN** the lane files under that project

#### Scenario: An unattributable lane is left out of the tree

- **WHEN** no worktree of a lane resolves to any project, directly or by
  sibling directory
- **THEN** the tree contains no group, lane or mirror row for it, and no
  top-level `Pipelines` group is created to hold it

#### Scenario: The flat layout grows no pipeline rows

- **WHEN** the sidebar is in the flat layout and the roster has lanes
- **THEN** no pipeline group, lane or mirror row is emitted

#### Scenario: The board is reachable without a sidebar row

- **WHEN** the sidebar renders no pipeline rows at all
- **THEN** `Action::OpenPipelineBoard` still opens the board
