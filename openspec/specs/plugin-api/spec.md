# plugin-api Specification

## Purpose

The plugin API is the versioned NDJSON contract between the host and out-of-process plugins (shell scripts or any language): manifest negotiation, request/response framing, lifecycle callbacks and host verbs. It is pinned by a committed JSON-schema snapshot so a wire change without a version bump fails the build, and every new field is additive so older plugins keep working.

## Requirements

### Requirement: The plugin wire is versioned by a committed schema snapshot

The plugin API contract (`plugin_api`) SHALL carry `API_VERSION`, and a committed JSON-schema snapshot of the wire types (`docs/api/plugin-api-<major>.<minor>.json`) MUST match the current types. A change to any wire type without a corresponding version bump and snapshot update MUST fail the test suite. The module is coverage-gated with the rest of `thegn-core`.

#### Scenario: Wire type changes without a bump

- **WHEN** a field is added to a wire type and `API_VERSION` is unchanged
- **THEN** the snapshot test fails and names the version file to regenerate

#### Scenario: Snapshot regenerates on request

- **WHEN** the snapshot test runs with the update environment variable set
- **THEN** it rewrites the snapshot for the current `API_VERSION`

### Requirement: Requests and responses are expressible on the wire

Plugin API v0.2 SHALL add a `RpcResponse` carrying `id` plus either `result` or an `RpcError { code, message, data }`, with `RpcErrorCode` ∈ {`unsupported`, `not_found`, `denied`, `auth`, `rate_limited`, `timeout`, `invalid`, `other`}, and a `Frame` that decodes one NDJSON line as either a message or a response. A bare `{"method": …}` line MUST still decode as a message.

#### Scenario: Response line decodes

- **WHEN** the line `{"id":7,"result":{"ok":true}}` is decoded as a `Frame`
- **THEN** it is a `Response` with id 7 and the given result

#### Scenario: Legacy message line decodes

- **WHEN** the line `{"method":"manifest"}` is decoded as a `Frame`
- **THEN** it is a `Message` with empty params

### Requirement: A plugin is declared by a loadable spec

`[[plugins]]` SHALL deserialize into a `PluginSpec` that flattens the manifest and adds `command` (argv, never a shell string), `cwd`, `env`, `timeout_secs`, `scopes` (the same scope lattice as control tokens), `mode` (`one_shot` | `resident`) and `enabled`. The spec is additive to v0.1 manifests: every new field has a default.

#### Scenario: Minimal spec parses

- **WHEN** a `[[plugins]]` entry provides only `id`, `name`, `version`, `api` and `command`
- **THEN** it parses with `mode = one_shot`, `enabled = true`, empty `scopes` and a default timeout

### Requirement: Host calls are a plugin verb

`HostVerb` SHALL include `host.call` carrying a capability id and params, `EventKind` SHALL include `action`, `worktree_changed`, `session_exit`, `notification` and a `custom` kind, and `Contribution` SHALL carry optional `caps` (JSON) and `chord`. Unknown values MUST continue to decode into the existing `Unknown`/default forms so a newer plugin does not break an older host.

#### Scenario: Unknown extension point still negotiates

- **WHEN** a manifest names an extension point this host does not know
- **THEN** the manifest parses with that contribution marked unknown and negotiation rejects only that contribution

### Requirement: The runtime honours the wire contract

The shipped runtime SHALL speak exactly the v0.2 wire shapes (`RpcMessage`, `RpcResponse`, `RpcError`, callback method names) pinned by the schema snapshot, and the bundled example plugin (`examples/plugins/hello.sh`) SHALL load and register through the real loader + apply path in a test.

#### Scenario: The example plugin round-trips

- **WHEN** the golden test runs `examples/plugins/hello.sh` through `spawn_ndjson` and applies its messages
- **THEN** its `register` and `update` land a renderable view on its statusbar surface

### Requirement: Plugin support facts have one canonical source

The plugin contract SHALL maintain one machine-readable support table covering
every extension point and host verb with its wire version, support state,
required surface capability or control scope, permitted mode, and owner for
reserved work. Loader negotiation, `plugin check`, schema/docs assertions, and
runtime dispatch MUST agree with it. Decoding an enum variant MUST NOT by itself
mark that surface supported.

#### Scenario: Reserved PanelSection is declared

- **WHEN** a v0.3 plugin requests `PanelSection` before THE-108 lands
- **THEN** decoding succeeds for compatibility but negotiation rejects the
  contribution with a stable reserved/unsupported diagnostic

#### Scenario: Documentation drifts from the host

- **WHEN** help calls a reserved extension point wired
- **THEN** the contract drift test fails naming that extension point

### Requirement: Plugin API v0.3 is additive over v0.2

The API version, committed JSON schema, wire documentation, and host negotiation
SHALL all identify v0.3. New multi-row and theme-slot fields SHALL default so a
v0.2 plugin's single-line view continues to decode, negotiate, serialize, and
render compatibly. Unsupported major/newer requirements SHALL fail with stable
diagnostics.

#### Scenario: v0.2 plugin connects to v0.3 host

- **WHEN** it registers a single-line role-styled view
- **THEN** the host accepts and handles it with the established v0.2 behavior

### Requirement: Exec scope is documented and enforced exactly

Plugin manifests and host calls SHALL treat `read`, `write`, `git`, and `exec`
as independent scopes, with `admin` implying all. A surface capability or plugin
registration MUST NOT grant `exec`; process-launching host capabilities require
the declared `exec` scope in the same central authorization lattice.

#### Scenario: Write-only plugin requests execution

- **WHEN** a plugin granted `write` but not `exec` calls an exec-scoped host
  capability
- **THEN** the host denies and audits the call with the stable permission error
