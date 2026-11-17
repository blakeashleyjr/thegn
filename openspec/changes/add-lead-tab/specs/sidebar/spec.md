# Sidebar

## ADDED Requirements

### Requirement: The Lead has its own top section

When `[lead]` is configured the sidebar SHALL render a Lead section above the
workspaces, backed by a terminal named `lead` of kind `lead`. Each tab SHALL be
a separate conversation.

#### Scenario: Lead not configured

- **WHEN** `[lead]` is absent
- **THEN** no Lead section is rendered
