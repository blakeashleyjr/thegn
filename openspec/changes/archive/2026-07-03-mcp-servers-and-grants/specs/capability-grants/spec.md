## ADDED Requirements

### Requirement: Capabilities are glob-scoped grants checked before side effects

thegn SHALL model a permission as a `Grant` with a kind (`process:exec`,
`download_file`, `npm:install`, `cargo:install`) and a glob scope, and expose a
pure `allows(action)` that returns true only when some grant of the matching
kind has a scope glob that matches the action's resource. The glob matcher MUST
support `*` (matches within a segment) and `**` (matches across segments), and
be pure/unit-tested. Grants gate the acquisition and launch of _user-declared_
tools; absent a matching grant, the action MUST be refused.

#### Scenario: A matching grant allows the action

- **WHEN** an action (e.g. `cargo:install` of `some-crate`) is checked against a
  grant of the same kind whose scope glob matches the resource
- **THEN** `allows` returns true and the action may proceed

#### Scenario: No matching grant refuses the action

- **WHEN** an action is checked and no grant of that kind matches its resource
- **THEN** `allows` returns false and the caller refuses the action with a clear
  message

#### Scenario: Glob scopes match by segment and recursively

- **WHEN** a grant scope uses `*` or `**`
- **THEN** `*` matches within one segment and `**` matches across segments, so a
  scope can be narrow (`npm:install` of `@scope/*`) or broad (`**`)
