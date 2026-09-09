# Work Tracking — tracker seam cheap-gap delta

## ADDED Requirements

### Requirement: Optional issue operations use shared typed seam errors

`IssueError` SHALL implement `thegn_core::seam::SeamError`. The optional issue
operations SHALL return a typed `Unsupported` error naming the operation when
the provider does not implement them, without performing I/O. Connect/timeout
failures SHALL remain `Transient`; authentication and ordinary API/parse
failures SHALL remain final classes.

#### Scenario: An unsupported optional operation is classified

- **WHEN** a provider does not implement an optional comment or label operation
- **THEN** it returns typed `Unsupported` naming the operation without performing I/O

### Requirement: Issue capabilities agree with optional operations

`IssueBackend` SHALL expose an `IssueCaps` value for its optional operations.
The offline conformance suite SHALL enumerate every `IssueProviderKind::ALL`
entry and check the false-cap default path, provider declarations, and
overclaim/underclaim test doubles without network or subprocess access. Native
provider tests SHALL cover any declared positive operation.

#### Scenario: Provider declarations match behavior

- **WHEN** the offline conformance suite enumerates every configured issue-provider kind
- **THEN** false capabilities refuse locally and declared positive capabilities have native behavior tests

### Requirement: Configured issue probes report capabilities

The issue probe registry SHALL attach the selected account's `IssueCaps` to its
`ProbeReport`, remain deterministic, avoid credentials, and never perform a
network round trip. Standalone doctor need not start live resident plugins.

#### Scenario: A configured probe is capability-only

- **WHEN** doctor builds a probe report for a configured issue account
- **THEN** the report includes its declared capabilities without reading credentials or making a network request
