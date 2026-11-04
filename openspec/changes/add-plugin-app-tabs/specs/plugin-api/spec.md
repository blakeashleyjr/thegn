## MODIFIED Requirements

### Requirement: Plugin API v0.3 is additive over v0.2

The v0.3 additions (multi-row views, theme slots, PanelSection vocabulary)
SHALL remain decodable by every later 0.x host. New multi-row and theme-slot
fields SHALL default so a v0.2 plugin's single-line view continues to decode,
negotiate, serialize, and render compatibly. Unsupported major/newer
requirements SHALL fail with stable diagnostics.

#### Scenario: v0.2 plugin connects to v0.3 host

- **WHEN** it registers a single-line role-styled view
- **THEN** the host accepts and handles it with the established v0.2 behavior

## ADDED Requirements

### Requirement: Plugin API v0.4 adds resident app tabs additively

The API version, committed JSON schema (`docs/api/plugin-api-0.4.json`), wire
documentation and host negotiation SHALL all identify v0.4.0. v0.4 SHALL add
the `AppTab` extension point (resident mode, on-demand cadence, required
capability `surface:app`), the `thegn_core::plugin_app` AppView/AppEvent wire
types, and the `background` spec permission. A manifest declaring API 0.3.x
MUST still negotiate and run unchanged; an `AppTab` contribution from a 0.3
manifest SHALL be rejected with the "requires API 0.4.0" diagnostic.

#### Scenario: A v0.3 statusbar plugin connects to a v0.4 host

- **WHEN** a plugin declaring `api = "0.3.0"` registers a statusbar view
- **THEN** it negotiates and renders exactly as before

#### Scenario: A v0.3 manifest declares an AppTab

- **WHEN** a plugin declaring `api = "0.3.0"` declares an `AppTab` contribution
- **THEN** that contribution is rejected because it requires API 0.4.0

### Requirement: Background work requires both a capability and user permission

`background:sync` SHALL be granted only when the manifest declares it, the
plugin is resident, and the user's spec sets `background = true`. Declaring it
SHALL still count as a non-surface grant that requires `trusted_native = true`.

#### Scenario: Declared but not permitted

- **WHEN** a resident plugin declares `background:sync` and its spec leaves
  `background = false`
- **THEN** the capability is denied and the plugin is told `background: false`

### Requirement: Resident apps carry explicit editing and mutation identities

App events SHALL carry the originating session token, surface, state scope and
presentation revision. Form events SHALL identify the editing instance and
draft version. Reliable actions SHALL echo plugin-authored captured bindings
and preconditions. Presentation revision MUST NOT substitute for a mutation
precondition.

#### Scenario: Selection changes before a submission is delivered

- **WHEN** a submission captured task A and the user later selects task B
- **THEN** the plugin applies or rejects the captured task A binding without
  retargeting it

### Requirement: Reliable app events acknowledge completed local acceptance

Reliable app events (action, activate, submit, cancel, invoke, sort) SHALL be
RPC requests with host-allocated ids answered by an `AppEventResult`. Local
admission failure SHALL be reported differently from an admitted request whose
outcome is unknown, and the host MUST NOT replay an unknown-outcome request
automatically.

#### Scenario: The session dies before the acknowledgment

- **WHEN** the plugin exits after a submit was admitted but before it answered
- **THEN** the host keeps the draft and an inspectable unknown-outcome record and
  sends no retry

### Requirement: App views and local editing are bounded

App views SHALL be validated before replacement: at most 12 nesting levels,
2048 widgets, 4096 rows, 64 fields, a 512x256 grid with at most 32768 cells,
and an encoded frame under 1 MiB. Grid text MUST NOT contain terminal control
sequences. The shared UI SHALL preserve editing state only within a matching
state scope and form instance and bound field size, undo history and recovery
storage (8 records, 8 MiB).

#### Scenario: A refresh removes a dirty form

- **WHEN** the new view omits the form or changes its editing identity
- **THEN** the old edit remains recoverable and is excluded from the new
  submission
