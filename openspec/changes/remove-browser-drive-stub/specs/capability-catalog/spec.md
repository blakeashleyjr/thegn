# Capability Catalog — browser.drive truthfulness delta

## MODIFIED Requirements

### Requirement: Stub capabilities are declared

A catalog row whose implementation unconditionally answers `Unimplemented`
SHALL carry a `stub` marker and appear distinctly in introspection while it is a
short-lived compatibility bridge. `browser.drive` SHALL NOT remain such a
bridge: because no browser-automation provider or success path exists, its
catalog row, verb, and transport projections MUST be absent. Reintroducing the
id requires at least one complete provider-backed implementation and the usual
surface coverage.

#### Scenario: browser.drive is not advertised

- **WHEN** clients enumerate capabilities or generated public schemas
- **THEN** `browser.drive` is absent rather than discoverable as an operation
  that always returns `Unimplemented`

#### Scenario: Working preview remains visible

- **WHEN** clients inspect the preview surface after the removal
- **THEN** implemented preview discovery/open/fetch capabilities retain their
  existing catalog and behavior
