## ADDED Requirements

### Requirement: Tracker identity syntax is admitted before dispatch

Issue provider adapters SHALL validate configured, caller-supplied, cached, and
provider-returned identities before using them as URL path segments, query
values, CLI options, or plugin control values. Built-in identities SHALL use
bounded provider syntax; plugin native keys SHALL use a separate bounded opaque
UTF-8 envelope. A malformed scoped identity MUST fail with a parse error and
MUST NOT silently fall back to a bare identifier.

#### Scenario: malformed scoped GitHub identity is refused

- **WHEN** a GitHub id contains an invalid repository or issue number
- **THEN** the adapter returns a parse error before invoking `gh`

#### Scenario: enterprise GitHub authority is preserved

- **WHEN** a provider response URL uses the configured `GH_HOST`
- **THEN** the adapter retains its validated owner/repo scope in the issue id

#### Scenario: plugin key remains opaque

- **WHEN** a plugin returns a bounded Unicode key containing a delimiter
- **THEN** the bridge accepts it in the plugin namespace and applies no
  built-in path-segment grammar to the business key

### Requirement: Control identities use one path encoding

The control client SHALL encode a complete issue identity once as one path
segment, and the control server SHALL decode it once before provider routing.
The implementation MUST NOT split or repeatedly decode the encoded identity.

#### Scenario: scoped identity remains one control segment

- **WHEN** a caller gets or updates `github:owner/repo#42`
- **THEN** the request path carries one encoded id and the provider receives
  the original identity exactly once
