# workspace Specification

## ADDED Requirements

### Requirement: The user-facing name for a workspace is "project"

Every user-facing surface SHALL name the workspace concept "project": the
sidebar section heading, action labels and hints, menus, modals, wizard and
status text, help-page titles and prose, `config.toml.example` prose, and CLI
help prose. Machine identifiers SHALL remain `workspace`: action ids,
database tables, control-API/CLI JSON field names, capability ids, and the
internal/spec vocabulary. Palette and help search MUST match both words, so
"workspace" keeps finding the renamed entries. In tracker-adjacent surfaces,
a tracker's own workspace/project concept MUST be qualified (e.g. "Linear
project") so the bare word "project" always denotes the thegn concept.

#### Scenario: The sidebar and palette say project

- **WHEN** the user opens the sidebar and the command palette
- **THEN** the section heading reads "PROJECTS" and the entries read "New
  project" / "Switch project", while the underlying action ids
  (`new-workspace`, `switch-workspace`, …) are unchanged

#### Scenario: Old vocabulary still searches

- **WHEN** the user types "workspace" into the command palette
- **THEN** the project actions (new/switch/delete) still match

#### Scenario: Keybind configs keep working

- **WHEN** a user config binds `new-workspace = "..."` under `[keybinds]`
- **THEN** the binding resolves exactly as before the rename
