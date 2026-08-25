# Configuration

## ADDED Requirements

### Requirement: Project spellings are accepted config aliases

The configuration SHALL accept `project` spellings as aliases of the
workspace-named keys — `projects_dir` for `workspaces_dir`,
`[project.<slug>]` for `[workspace.<slug>]`, `confirm_delete_project` for
`confirm_delete_workspace`, and `sidebar_project_sort` for
`sidebar_workspace_sort` — with the `project` spellings documented as
canonical in `config/config.toml.example` and the `workspace` spellings
accepted indefinitely. When both spellings of one key are set, the `project`
spelling MUST win and `thegn config validate` MUST warn, naming both
locations. Strict validation's unknown-key report SHALL offer did-you-mean
suggestions from `project*` near-misses to the accepted key. The Rust struct
field names (the schema) and the derived home-manager module SHALL remain on
the internal `workspace` names. Tracker account keys that name a tracker's
own workspace/project concept (`workspace_id`, `workspace_slug`,
`project_id`, …) MUST NOT be aliased or renamed.

#### Scenario: A project-spelled config loads

- **WHEN** a config sets `projects_dir = "~/code"` and a
  `[project.myrepo.merge_queue]` overlay
- **THEN** they take effect exactly as the `workspaces_dir` /
  `[workspace.myrepo.merge_queue]` spellings would

#### Scenario: Both spellings set warns and the project spelling wins

- **WHEN** a config sets both `workspaces_dir` and `projects_dir` to
  different values
- **THEN** `projects_dir` takes effect and `thegn config validate` reports
  the duplicate, naming both keys

#### Scenario: Tracker keys are untouched

- **WHEN** a `[[tracker]]` account sets `workspace_id`
- **THEN** it parses as before — no alias, no warning — because it names the
  tracker's concept, not thegn's
