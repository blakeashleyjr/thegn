# Editor Specification (delta)

## ADDED Requirements

### Requirement: Editor targets include projects and source positions

The editor seam SHALL accept a project root, a file, or a file with optional
line and column. Project opens SHALL use each editor profile's project launch
form; terminal editors SHALL retain pane placement with the project root as
their working directory.

#### Scenario: Open a project in a GUI editor

- **WHEN** a worktree project target is opened with a GUI editor profile
- **THEN** the resolved launch receives the quoted worktree root and uses
  external placement

#### Scenario: Open a source position

- **WHEN** a file target includes line and column
- **THEN** the editor profile receives the position through its supported argv
  form without shell interpolation

### Requirement: All UI handoffs use the editor provider seam

Sidebar, diff, pull-request, and palette editor actions SHALL resolve and launch
through the same editor provider seam, honoring configured resolution and
external-versus-pane placement.

#### Scenario: A sidebar project handoff honors an override

- **WHEN** the targeted worktree has an editor override
- **THEN** the shared provider opens that worktree with the override rather than
  a UI-specific default
