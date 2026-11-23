# Sidebar

## MODIFIED Requirements

### Requirement: Pipeline rows only ever nest under a project

The sidebar SHALL render derived pipeline folders only inside the project that
owns their worktrees. No pipeline-shaped row — group, lane, worktree leaf, or
roster rollup — SHALL be emitted at the root of the tree.

In grouped layout, each pipeline-owned worktree SHALL appear once under its
owning project's `Pipelines` folder by default, rather than also appearing in
ordinary loose, filed, or Other rows. Ownership SHALL use durable persisted
nonblank stage paths independently of the bounded live roster, including
legacy-held stage rows without artifacts. A worktree whose persisted selected
environment is exactly the reserved `pipeline` environment SHALL also be filed
under Pipelines while no stage row exists. Other environment names SHALL NOT
establish pipeline ownership. Roster stage references and environment
ownership for one normalized path SHALL produce one actionable leaf. The leaf
SHALL share the ordinary worktree's rendering, Info, attention navigation,
quick-jump, and exact-target action behavior. Its pin identity SHALL remain the
existing `{project}/{worktree label}` key across grouped and flat layouts and
ownership changes, preserving existing pins. An explicit user folder
assignment MAY override the default Pipelines filing.

A lane's project SHALL be resolved from the first of its worktrees that
resolves through live session groups or DB-registered worktrees; when none does,
it SHALL be resolved from the directory holding that project's other
worktrees, and a directory claimed by more than one project SHALL be treated as
ambiguous and ignored. A lane that resolves to no project SHALL contribute no
rows to the tree; it remains on the pipeline board, which is the complete view
of the roster.

The leaf SHALL retain the ordinary worktree's target and complete status
rendering so activation, worktree actions, filtering, and cursor anchoring
continue to work. The leaf's context-menu **Move to Folder** action and a drag
onto a folder SHALL file that selected worktree through the existing folder
assignment path. The manual sibling reorder shortcuts SHALL NOT rewrite the
workspace's worktree order for a derived pipeline leaf; pipeline leaves retain
their deterministic lane order (earliest dispatch reference, then name). If that
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

#### Scenario: Legacy-held stage ownership survives without an artifact

- **WHEN** a worktree has a persisted nonblank legacy-held stage row without an
  artifact and no longer appears in the sidebar roster window
- **THEN** its open worktree still appears under Pipelines with a stable
  path-keyed lane and can be activated

#### Scenario: Native pipeline environment precedes stage metadata

- **WHEN** a worktree has the persisted reserved `pipeline` environment but its
  native dispatch has no stage or artifact metadata yet
- **THEN** its open worktree appears once under Pipelines and remains
  activatable

#### Scenario: Other persisted environments remain ordinary

- **WHEN** a worktree's persisted environment has a name other than `pipeline`
- **THEN** that environment alone does not move it under Pipelines

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

#### Scenario: Pipeline leaves keep ordinary worktree interactions

- **WHEN** a pipeline-owned worktree appears under a lane
- **THEN** it shows the ordinary activity, Git, PR, environment, and detail
  fields, supports Info and attention navigation, and receives its Alt-number
  slot

#### Scenario: Pipeline leaf folder actions target the selected worktree

- **WHEN** the user chooses **Move to Folder** on a pipeline leaf or drags that
  leaf onto a folder
- **THEN** the folder assignment applies to that leaf's worktree, which then
  appears in the explicit folder

#### Scenario: Manual reorder does not change derived pipeline order

- **WHEN** the user invokes a manual sibling reorder shortcut while a derived
  pipeline leaf is selected
- **THEN** the action does not rewrite manual workspace or folder worktree
  positions, and the leaf remains in deterministic dispatch order

#### Scenario: Existing pins survive layout and ownership changes

- **WHEN** an ordinary worktree is already pinned and becomes pipeline-owned,
  or the user switches between grouped and flat layouts
- **THEN** its existing pin continues to identify the same worktree

#### Scenario: An unattributable lane is left out of the tree

- **WHEN** no worktree of a lane resolves to any project, directly or by
  sibling directory
- **THEN** the tree contains no group, lane, or leaf row for it, and no
  top-level `Pipelines` group is created to hold it

#### Scenario: The board is reachable without a sidebar row

- **WHEN** the sidebar renders no pipeline rows
- **THEN** `Action::OpenPipelineBoard` still opens the board
