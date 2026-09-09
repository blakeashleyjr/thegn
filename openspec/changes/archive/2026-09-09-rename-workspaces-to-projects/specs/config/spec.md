# Configuration — project vocabulary delta

## ADDED Requirements

### Requirement: Project spellings are canonical with bounded compatibility

Configuration SHALL document and emit `projects_dir`, `[project.<slug>]`,
`confirm_delete_project`, and `sidebar_project_sort` as canonical spellings.
Their workspace-named predecessors SHALL remain accepted with deprecation
warnings for three stable releases under a named removal policy. When both
forms are present, the canonical value MUST win and validation MUST identify
both locations. `THEGN_PROJECTS_DIR` and its legacy environment alias SHALL
follow the same rule. Home Manager SHALL expose a canonical project option and
retain a deprecated compatibility option during the window.

#### Scenario: Legacy config remains loadable

- **WHEN** a user loads a workspace-spelled key during the compatibility window
- **THEN** it retains its old behavior and a diagnostic names the canonical
  replacement and removal policy

#### Scenario: Canonical form wins a duplicate

- **WHEN** both `projects_dir` and `workspaces_dir` are set
- **THEN** `projects_dir` takes effect and validation reports both spellings

### Requirement: Provider and machine config terms remain distinct

Tracker-owned `workspace_id`, `workspace_slug`, and `project_id` fields and
internal/storage schema terms MUST NOT be rewritten by project-vocabulary
normalization.

#### Scenario: Tracker workspace id is configured

- **WHEN** a tracker account sets `workspace_id`
- **THEN** it parses unchanged and receives no thegn project-alias diagnostic
