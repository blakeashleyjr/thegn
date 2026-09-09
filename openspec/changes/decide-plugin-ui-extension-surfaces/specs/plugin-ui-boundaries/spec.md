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

### Requirement: PanelSection remains separately owned

This decision SHALL NOT redefine plugin `PanelSection` negotiation, rendering,
cache/degradation, placement, or row activation; those behaviors remain owned by
THE-108 and the `add-ui-component-contract` change.

#### Scenario: Decision scope is reviewed

- **WHEN** THE-107 artifacts are compared with THE-108
- **THEN** SidebarTab, Theme, and key-zone decisions contain no competing
  PanelSection runtime contract
