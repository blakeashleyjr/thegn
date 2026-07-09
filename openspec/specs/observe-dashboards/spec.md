# Observe Dashboards

## Purpose

Dashboards give Observe a persistent, hand-editable definition of panels, layout, variables, and time defaults. They live as native TOML files loaded from a directory, are listable and switchable from the command palette, and can be edited and reloaded from within the TUI. Grafana JSON import is an opt-in, lossy convenience; export is a non-goal.

## Requirements

### Requirement: Native TOML dashboard format loaded from a directory

[M] Dashboards SHALL be modeled as serde structs (panels, layout, variables, time defaults, refresh) persisted in a native hand-editable TOML format — explicitly not Grafana JSON — and loaded from a directory, listable/switchable via the command palette.

#### Scenario: Load dashboards from a directory

- **WHEN** Observe starts with a dashboards directory configured
- **THEN** each dashboard file is loaded and selectable from the palette

### Requirement: In-TUI editing and reload

[S] Dashboards SHALL be editable/saveable from within the TUI (written back to file) and [S] reloaded on file change.

#### Scenario: Dashboard file changes on disk

- **WHEN** a dashboard file is edited externally
- **THEN** Observe reloads it without a restart

### Requirement: Grafana JSON import is opt-in and lossy; export is excluded

[C] Grafana dashboard-JSON import SHALL be a best-effort, clean-subset, lossy operation performed only on demand behind a flag; [W] Grafana JSON export / round-trip compatibility is a non-goal.

#### Scenario: Import a Grafana dashboard

- **WHEN** the user imports a Grafana JSON dashboard
- **THEN** the supported subset is converted to native TOML and unsupported parts
  are reported as dropped
