# Capability Catalog — browser.drive truthfulness delta

## REMOVED Requirements

### Requirement: Stub capabilities are declared

A catalog row whose implementation unconditionally answers `Unimplemented`
SHALL carry a `stub` marker naming what it waits on; `thegn api list` and the
coverage report MUST present stub rows distinctly so a routed stub never
counts as a working capability. Removing the last `Unimplemented` answer for
a row MUST remove its marker.

#### Scenario: browser.drive reads as a stub

- **WHEN** `thegn api list` runs while `browser.drive` answers
  `Unimplemented` on every surface
- **THEN** the row is presented as a stub, and the coverage report counts it
  under stubs rather than plain implemented

## ADDED Requirements

### Requirement: Stub capabilities remain truthful

A catalog row whose implementation unconditionally answers `Unimplemented`
SHALL carry a `stub` marker and appear distinctly in introspection while it is a
short-lived compatibility bridge. Removing the last `Unimplemented` answer for
a row MUST remove its marker. A capability with no provider or success path
MUST NOT remain indefinitely as a stub: it SHALL be removed until a complete
implementation exists. Because no browser-automation provider or success path
exists, the `browser.drive` catalog row, verb, and transport projections MUST
be absent. Reintroducing the id requires at least one complete provider-backed
implementation and the usual surface coverage.

#### Scenario: browser.drive is not advertised

- **WHEN** clients enumerate capabilities or generated public schemas
- **THEN** `browser.drive` is absent rather than discoverable as an operation
  that always returns `Unimplemented`

#### Scenario: Working preview remains visible

- **WHEN** clients inspect the preview surface after the removal
- **THEN** implemented preview discovery/open/fetch capabilities retain their
  existing catalog and behavior
