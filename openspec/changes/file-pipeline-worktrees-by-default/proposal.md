# File pipeline worktrees under Pipelines by default

## Why

The sidebar currently shows pipeline-owned worktrees in the ordinary project
list and again as leaves under `Pipelines`. The duplicate rows make pipeline
worktrees look like ordinary checkouts and make the Pipelines folder less
useful as their default home.

## What Changes

- In grouped layout, each pipeline-owned worktree appears once as a worktree
  leaf under its owning project's `Pipelines` folder.
- Ownership comes from the unbounded persisted staged-dispatch paths, so an
  open pipeline worktree remains filed after its row leaves the sidebar roster.
- Worktree paths are compared and keyed by stable lexical path identity;
  repeated stage references collapse to one leaf.
- Explicit folder filing remains an override. Flat layout keeps its ordinary
  flat rows and emits no derived pipeline folders.
- Pipeline leaves retain worktree targets, status, selection, filtering, and
  ordinary keyboard/mouse actions. An active pipeline worktree reveals its
  collapsed Pipelines/lane ancestors without changing persisted collapse state.

## Impact

- **Specs:** `sidebar` — pipeline worktree filing, ownership, deduplication,
  and interaction behavior.
- **Code:** `sidebar.rs`, `sidebar_pipeline.rs`, `attention_status.rs`, sidebar
  key handling, and sidebar cursor re-anchoring.
- **Proof:** focused host sidebar model/interaction tests are added; execution
  and Mac visual proof are left for the coordinator because this worktree
  forbids local builds and tests.
