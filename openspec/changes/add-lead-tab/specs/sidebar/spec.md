# Sidebar

## ADDED Requirements

### Requirement: The Lead has its own top section

When `[lead]` is configured the sidebar SHALL render a Lead section above the
workspaces, backed by a terminal named `lead` of kind `lead`. Each tab SHALL be
a separate conversation.

Activating the Lead row by mouse SHALL use the same Lead entry path as the jump
key. It SHALL focus the Lead tab, open setup when the Lead is not ready, or
leave a launch error and its recovery action visible on the Lead tab.

#### Scenario: Lead not configured

- **WHEN** `[lead]` is absent
- **THEN** no Lead section is rendered

#### Scenario: Click opens the global Lead

- **WHEN** the Lead row is clicked while the Lead is ready
- **THEN** the Lead tab is focused and starts or resumes its conversation

#### Scenario: Click opens setup or a recoverable launch error

- **WHEN** the Lead row is clicked while setup is needed or launch fails
- **THEN** setup opens, or the error and recovery action remain visible with the Lead tab focused
