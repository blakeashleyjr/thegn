# Plugin Runtime — tracker capability delta

## ADDED Requirements

### Requirement: Plugin issue capabilities are declared and enforced locally

An accepted `IssueProvider` contribution MAY carry the issue `caps` object.
Omitted or null caps SHALL mean all false, and unknown cap fields SHALL be
rejected. The host SHALL pass the declaration to `PluginIssueBackend`.
Operations behind a false cap SHALL return typed `Unsupported` without a
`provider.call` round trip. Operations behind a true cap SHALL use the existing
`provider.call` wire (`seam = "issues"`, operation name, serialized args), and
an upstream `unsupported` reply SHALL map to typed `Unsupported` as a second
degradation boundary. The existing five core operations, router composition,
timeouts, and old manifests remain compatible.

#### Scenario: An omitted optional capability fails locally

- **WHEN** an accepted `IssueProvider` omits `caps` and the host requests an optional comment operation
- **THEN** the bridge returns typed `Unsupported` without sending a `provider.call`

#### Scenario: A declared capability forwards through the bridge

- **WHEN** an accepted `IssueProvider` declares `caps.comments = true` and the host requests a comment operation
- **THEN** the bridge sends the existing issues `provider.call` and maps an upstream `unsupported` reply back to typed `Unsupported`

### Requirement: Plugin tracker caps share native conformance

The offline issue conformance suite SHALL cover a plugin bridge with omitted,
false, and true capability declarations using a scripted fixture. It SHALL
prove false-cap local refusal and true-cap forwarding without making a network
request. Standalone `thegn doctor` is not required to start resident plugins;
plugin manifest inventory is a follow-up.

#### Scenario: Plugin capability conformance stays offline

- **WHEN** the conformance suite exercises omitted, false, and true plugin issue capabilities
- **THEN** a scripted fixture proves local refusal and forwarding without network access
