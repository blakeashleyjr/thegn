# Work Tracking — tracker seam cheap-gap delta

### Requirement: Optional issue operations use shared typed seam errors

`IssueError` SHALL implement `thegn_core::seam::SeamError`. The optional issue
operations SHALL return a typed `Unsupported` error naming the operation when
the provider does not implement them, without performing I/O. Connect/timeout
failures SHALL remain `Transient`; authentication and ordinary API/parse
failures SHALL remain final classes.

### Requirement: Issue capabilities agree with optional operations

`IssueBackend` SHALL expose an `IssueCaps` value for its optional operations.
The offline conformance suite SHALL enumerate every `IssueProviderKind::ALL`
entry and check the false-cap default path, provider declarations, and
overclaim/underclaim test doubles without network or subprocess access. Native
provider tests SHALL cover any declared positive operation.

### Requirement: Configured issue probes report capabilities

The issue probe registry SHALL attach the selected account's `IssueCaps` to its
`ProbeReport`, remain deterministic, avoid credentials, and never perform a
network round trip. Standalone doctor need not start live resident plugins.
