# Plugin UI Boundaries

## ADDED Requirements

### Requirement: Optional plugin UI surfaces require explicit decisions

`SidebarTab`, `Theme`, and plugin key-zone contributions SHALL each have an
explicit adopt, defer, or reject state with rationale. Public documentation,
schemas, and negotiation MUST agree with that state; wire vocabulary alone MUST
NOT be described as runtime support.

#### Scenario: Reserved enum is inspected

- **WHEN** a host understands the serialized `SidebarTab` name but has no wired
  sidebar runtime
- **THEN** negotiation rejects it and support documentation marks it reserved
  rather than usable

### Requirement: Adopted surfaces are host-bounded

Before any of these surfaces becomes supported, its implementation change SHALL
define stable contribution/action ids, version negotiation, capabilities and
scopes, host/plugin state ownership, placement/visibility, layout and work
budgets, input routing, failure degradation, terminal sanitization, help, and
backward compatibility. Plugins MUST NOT emit terminal control bytes, dispatch
arbitrary host actions, or introduce their own global key conflict policy.

#### Scenario: Plugin contribution exceeds its budget

- **WHEN** an adopted UI contribution supplies excessive content or work
- **THEN** the host truncates, throttles, rejects, or degrades it within the
  published bound while preserving UI responsiveness

### Requirement: Adopted SidebarTab contributions are host-rendered rows

An implemented `SidebarTab` SHALL contribute only bounded rows rendered by the
host from semantic `View`/`Span`/`StyleRole` data, with optional badges. The
host MUST retain ownership of native navigation, sidebar mode, layout,
selection, scrolling, hit targets, narrow-layout truncation, and focus
restoration. Plugin rows MUST follow native navigation and MUST activate only
a declared, namespaced plugin action. Row-count and per-row byte limits SHALL
be set from a measured budget by the implementation change. A crash, removal,
or API-version mismatch MUST remove that plugin's rows without changing
sidebar mode.

#### Scenario: A plugin crashes while its sidebar rows are visible

- **WHEN** the host detects that a contributing plugin has crashed
- **THEN** the host removes its rows and restores focus safely while preserving
  the current sidebar mode and all other rows

### Requirement: Adopted Theme contributions are complete data-only palettes

An implemented `Theme` contribution SHALL use the complete closed `UserTheme`
shape and the existing importer validation, contrast audit, and terminal
capability token resolution. The host MUST require `surface:theme`, enforce a
size cap no larger than the existing theme-file limit, show plugin provenance,
and expose the palette only through normal user theme selection. A contribution
MUST NOT be executable styling, contain terminal escape sequences, or be
auto-applied. If a selected plugin theme is removed, the host MUST select the
default preset and show a status message.

#### Scenario: A theme contribution omits a required token

- **WHEN** a plugin submits a palette that does not cover the complete
  `UserTheme` shape
- **THEN** the host rejects the contribution and does not expose or apply it

### Requirement: Adopted key zones are limited to a plugin's focused surfaces

An implemented plugin key zone SHALL declare versioned namespaced action ids
and suggested chords only for that plugin's own focused sidebar rows or
statusbar segment. The host MUST own conflict resolution, focus attribution,
dispatch, help and which-key projection, and user overrides. A host or user
binding MUST take precedence over a conflicting plugin chord. Plugins MUST
NOT bind global chords, intercept pane input, dispatch undeclared host actions,
or bind reserved escape, focus, or safety chords. Removal or API-version
mismatch MUST remove the plugin's bindings without changing host or user
bindings.

#### Scenario: A suggested chord conflicts with a host binding

- **WHEN** a plugin suggests a chord already bound by the host or user
- **THEN** the plugin chord remains unbound, the existing binding still
  dispatches, and the conflict is reported

### Requirement: PanelSection remains separately owned

This decision SHALL NOT redefine plugin `PanelSection` negotiation, rendering,
cache/degradation, placement, or row activation; those behaviors remain owned by
THE-108 and the `add-ui-component-contract` change.

#### Scenario: Decision scope is reviewed

- **WHEN** THE-107 artifacts are compared with THE-108
- **THEN** SidebarTab, Theme, and key-zone decisions contain no competing
  PanelSection runtime contract
