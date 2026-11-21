# Design

The DB's unbounded `pipeline_worktree_paths` query is the ownership source;
the bounded roster continues to supply issue lane names. Paths are normalized
lexically before ownership, target resolution, row identity, and deduplication.
Paths absent from the roster receive a stable path-keyed lane. A path referenced
by multiple roster lanes is assigned to its first stable lane only.

Grouped rows remove pipeline-owned worktrees from ordinary loose, filed, and
Other placements, then emit a worktree row under Pipelines with the same
hydrated `Group` fields and `RowTarget` used by the primary row. A user's
explicit DB folder assignment removes that path from the default ownership set.
The flat builder does not apply this filtering and continues to omit pipeline
folders.

An active pipeline worktree temporarily opens collapsed pipeline ancestors for
rendering. The persisted `ViewState::collapsed` keys are not changed. The row's
stable path key and retained `worktree_path` let cursor re-anchoring survive the
transition from ordinary row to Pipelines leaf.
