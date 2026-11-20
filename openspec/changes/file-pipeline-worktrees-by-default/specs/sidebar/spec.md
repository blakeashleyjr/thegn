# Sidebar

## MODIFIED Requirements

### Requirement: Pipeline rows only ever nest under a project

The sidebar SHALL render derived pipeline folders only inside the project that
owns their worktrees. No pipeline-shaped row — group, lane, worktree leaf, or
roster rollup — SHALL be emitted at the root of the tree.

In grouped layout, each pipeline-owned worktree SHALL appear once under its
owning project's `Pipelines` folder by default, rather than also appearing in
ordinary loose, filed, or Other rows. Ownership SHALL use durable persisted
staged-dispatch paths independently of the bounded live roster. Roster stage
references to one normalized path SHALL produce one leaf with stable path
identity. An explicit user folder assignment MAY override the default
Pipelines filing.

A lane's project SHALL be resolved from the first of its worktrees that
resolves through live session groups or DB-registered worktrees; when none does,
it SHALL be resolved from the directory holding that project's other
worktrees, and a directory claimed by more than one project SHALL be treated as
ambiguous and ignored. A lane that resolves to no project SHALL contribute no
rows to the tree; it remains on the pipeline board, which is the complete view
of the roster.

The leaf SHALL retain the ordinary worktree's target and status so activation,
worktree actions, filtering, and cursor anchoring continue to work. If that
worktree is the active session worktree, its Pipelines and lane ancestors SHALL
be rendered open for that frame even when their persisted collapsed keys are
set; the collapsed state itself MUST NOT be changed. The flat layout has no
project rows to nest under and SHALL therefore emit no pipeline folders or
leaves, while retaining its ordinary flat worktree rows.

The pipeline board SHALL remain reachable independently of any sidebar row,
via `Action::OpenPipelineBoard`.

#### Scenario: A lane nests under the project owning its worktrees

- **WHEN** a lane's worktree resolves to a registered project
- **THEN** its `Pipelines` group, lane folder, and worktree leaves render inside
  that project's subtree, and none of them at depth 0

#### Scenario: Persisted ownership outlives the bounded roster

- **WHEN** a worktree remains in persisted staged-dispatch rows but no longer
  appears in the sidebar roster window
- **THEN** its open worktree still appears under Pipelines with a stable
  path-keyed lane and can be activated

#### Scenario: Repeated stage references are deduplicated

- **WHEN** multiple stages or path spellings refer to the same pipeline
  worktree
- **THEN** the sidebar emits one leaf for that normalized worktree path

#### Scenario: Explicit folder filing wins

- **WHEN** a user files a pipeline-owned worktree into a project folder
- **THEN** it remains in that folder instead of being moved back to Pipelines

#### Scenario: Active pipeline worktree survives collapsed ancestors

- **WHEN** the active worktree is under Pipelines and its group or lane has a
  persisted collapsed key
- **THEN** the active leaf remains visible while the persisted collapse keys
  remain unchanged

#### Scenario: Flat layout stays flat

- **WHEN** the user selects flat layout
- **THEN** pipeline worktrees remain ordinary rows in the flat list and no
  pipeline group, lane, or leaf rows are emitted

#### Scenario: An unattributable lane is left out of the tree

- **WHEN** no worktree of a lane resolves to any project, directly or by
  sibling directory
- **THEN** the tree contains no group, lane, or leaf row for it, and no
  top-level `Pipelines` group is created to hold it

#### Scenario: The board is reachable without a sidebar row

- **WHEN** the sidebar renders no pipeline rows
- **THEN** `Action::OpenPipelineBoard` still opens the board
