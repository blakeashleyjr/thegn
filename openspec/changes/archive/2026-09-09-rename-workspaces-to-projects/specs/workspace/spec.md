# Workspace — project presentation delta

## ADDED Requirements

### Requirement: One-repository workspace presentation uses project vocabulary

Sidebar chrome, palette/action labels, prompts, menus, help, configuration
prose, and human CLI help SHALL call thegn's one-repository object a project.
Canonical action ids SHALL use project spellings, while old workspace action ids
remain accepted compatibility aliases and workspace terms remain searchable.
Machine JSON/state identifiers remain stable.

#### Scenario: Project action uses an old keybinding

- **WHEN** existing configuration binds the legacy workspace action id
- **THEN** the canonical project action dispatches with the same chord and no
  duplicate palette row

### Requirement: Multi-repository groups use program vocabulary

The existing multi-repository CLI namespace and capability ids SHALL use
`program`. Exact legacy `project` command/flag/capability forms SHALL retain
their behavior as deprecated compatibility aliases during the bounded window,
with diagnostics excluded from machine-readable output.

#### Scenario: Legacy multi-repo command runs

- **WHEN** a user invokes the former `thegn project` command
- **THEN** the corresponding `thegn program` behavior runs and a human warning
  identifies the canonical spelling

#### Scenario: JSON remains stable

- **WHEN** a compatibility alias is used with machine-readable output
- **THEN** the documented JSON schema/fields are unchanged and no warning is
  mixed into the JSON stream
