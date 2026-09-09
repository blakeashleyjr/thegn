# Distribution

## ADDED Requirements

### Requirement: Package outputs derive from one deterministic release contract

The release renderer SHALL consume `packaging/release.json` as the source of
truth for archive naming/layout, enabled targets, manager templates, and output
paths. It SHALL render Homebrew, AUR, and nfpm deb/rpm metadata deterministically
and reject missing, unexpected, inconsistent, or unsafe artifact inputs.
Windows, Scoop, and winget SHALL remain explicitly disabled until a separate
accepted change enables and verifies their target artifacts.

#### Scenario: The same release inputs render identically

- **WHEN** the renderer receives the same tag, checksums, manifest, and templates
- **THEN** every output is byte-identical and belongs to the declared output set

#### Scenario: An undeclared artifact fails closed

- **WHEN** downloaded assets do not match the enabled target/checksum contract
- **THEN** packaging stops before rendering or publishing manager metadata

### Requirement: Release assets carry checksums and keyless provenance

The tag workflow SHALL upload the contracted archives/packages with checksum
assets and GitHub OIDC build-provenance attestations. Verification MUST NOT
depend on a long-lived artifact-signing private key, and package metadata MUST
resolve only assets produced for the same release contract.

#### Scenario: A release archive is verifiable

- **WHEN** a user verifies an uploaded archive with GitHub attestation tooling
- **THEN** the attestation identifies this repository's release workflow and
  its checksum matches the published checksum asset

### Requirement: Homebrew and AUR publication is protected and idempotent

Homebrew/AUR publication SHALL run only through the protected publication
environment after validating public-repository and narrowly scoped deploy-key
configuration. It SHALL publish the exact rendered metadata, regenerate AUR
`.SRCINFO`, avoid exposing credentials through argv/logs/artifacts, and push
only when content differs. Failure MUST NOT alter/retract release assets and a
same-tag rerun MUST be idempotent.

#### Scenario: Missing setup blocks publication clearly

- **WHEN** a tap/package repository or deploy credential is absent
- **THEN** the protected job fails with a setup checklist before attempting a
  metadata push

#### Scenario: A rerun has no duplicate update

- **WHEN** published metadata already equals the rendered release output
- **THEN** the job performs no new commit and succeeds without changing assets

### Requirement: A channel is advertised only after real rehearsal

Installation documentation SHALL advertise a package channel only after a
recorded real-release rehearsal has published (where applicable), installed on
a clean representative system, verified version/checksum/provenance, and tested
relevant upgrade/uninstall behavior. Template/render success alone MUST NOT be
treated as channel support.

#### Scenario: Generated Homebrew metadata is still pending

- **WHEN** the formula renders but the public tap install has not been rehearsed
- **THEN** docs mark the channel pending rather than presenting it as supported

### Requirement: Deferred managers have explicit product decisions

Windows Scoop/winget SHALL be considered only after Windows release artifacts
are enabled and green. Hosted apt/rpm repositories and crates.io publication
SHALL remain non-goals until a product decision accepts their operational and
workspace-publication costs; accepted work MUST be split into bounded tracked
changes. Package-manager publication SHALL use the stable channel only.

#### Scenario: A disabled manager cannot be published

- **WHEN** a manager or its target is disabled in the release manifest
- **THEN** renderer/workflow publication produces no output for that manager
