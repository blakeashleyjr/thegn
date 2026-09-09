# Sidebar

## ADDED Requirements

### Requirement: Submodule state has a distinct optional indicator

The sidebar SHALL be able to render a separate submodule-dirty/conflicted
indicator from the cached Git read model, controlled by its `[ui]` visibility
setting. Disabling that indicator MUST NOT change Git state or other dirty
signals.

#### Scenario: Indicator is hidden by preference

- **WHEN** the submodule-status visibility setting is disabled
- **THEN** the submodule glyph is omitted while all underlying state and other
  sidebar indicators remain unchanged
