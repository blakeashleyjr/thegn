# plugin-api — delta for add-ui-component-contract

## ADDED Requirements

### Requirement: Plugin API v0.3 carries multi-row views and theme-slot styling

Plugin API v0.3 SHALL extend `View` with optional multi-row content (rows of spans; the existing single-line `spans` stays the compat path an older plugin keeps using) and extend `Span` with an optional theme-slot name resolved against the host's token vocabulary, with the existing `StyleRole` as the fallback when the name is absent or unknown to this host. `PanelSection` SHALL join the extension-point vocabulary as a wired surface (`SidebarTab` and `Theme` remain declared-but-unsupported). Every addition MUST be additive per the versioning requirement: `API_VERSION` bumps, the committed schema snapshot regenerates, every new field defaults, and a v0.2 plugin or an older host keeps working unchanged.

#### Scenario: A v0.2 view still renders

- **WHEN** a plugin sends a view with only the single-line `spans` field and role styling
- **THEN** the host renders it exactly as before the bump

#### Scenario: An unknown slot name falls back to the role

- **WHEN** a span names a theme slot this host's vocabulary does not include
- **THEN** the span renders with its `StyleRole` styling and the view is not rejected

#### Scenario: The wire change is snapshot-pinned

- **WHEN** the v0.3 fields land without the `API_VERSION` bump and snapshot update
- **THEN** the schema snapshot test fails naming the version file to regenerate
