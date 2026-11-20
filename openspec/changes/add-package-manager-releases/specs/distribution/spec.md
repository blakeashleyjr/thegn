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

### Requirement: Release downloads have a selected aggregate byte budget

Before downloading release assets, the workflow SHALL validate each selected
asset against its existing per-asset limit and validate the selected total
against a 1 GiB aggregate limit. Pattern-filtered downloads SHALL count only
assets selected by those patterns; an unfiltered manifest download SHALL count
all listed release assets. The aggregate check MUST reject excess before the
download command runs.

#### Scenario: Selected assets exceed the aggregate budget

- **WHEN** each selected release asset is within its per-asset limit but their
  combined size exceeds 1 GiB
- **THEN** the workflow refuses the download before creating the package input
  set

### Requirement: Homebrew and AUR publication is protected and idempotent

Homebrew/AUR publication SHALL run only through the protected publication
environment after validating public-repository and narrowly scoped deploy-key
configuration. It SHALL publish the exact rendered metadata, regenerate AUR
`.SRCINFO`, avoid exposing credentials through argv/logs/artifacts, and push
only when content differs. SSH endpoints SHALL match committed, reviewed
`known_hosts` keys. Failure MUST NOT alter/retract release assets and a same-tag
rerun MUST be idempotent.

Each successful target publication SHALL upload a credential-free,
machine-readable receipt artifact. The receipt SHALL bind the signed release
manifest digest, admitted tag/source identity, release workflow run and
attempt, publication workflow run and attempt, target, outcome, and exact old,
new, and observed external commit OIDs. It SHALL record each published
repository-relative path and the SHA-256 of its exact blob. Homebrew formula
and AUR PKGBUILD digests SHALL match signed manifest entries. The generated AUR
`.SRCINFO` digest SHALL match a workflow-run-bound sidecar from the exact
immutable artifact ID. Deploy keys MUST be removed before receipt generation,
and only the receipt may be uploaded after workspace cleanup. Receipt artifacts
are retained for 90 days; longer-term archival requires a separate policy.

Before either publisher materializes a deploy key, it SHALL reverify the signed
release manifest against the live approved annotated tag object and peeled
commit, and against the exact successful release workflow run, attempt, and
source commit recorded by the attestation. A mismatch MUST refuse publication.

#### Scenario: Published release identity has changed or cannot be verified

- **WHEN** the manifest signature, live annotated tag identity, or exact
  successful workflow run/attempt/source identity does not match
- **THEN** publication stops before deploy-key materialization and makes no
  package-manager update

#### Scenario: Missing setup blocks publication clearly

- **WHEN** a tap/package repository or deploy credential is absent
- **THEN** the protected job fails with a setup checklist before attempting a
  metadata push

#### Scenario: A rerun has no duplicate update

- **WHEN** published metadata already equals the rendered release output
- **THEN** the job performs no new commit and succeeds without changing assets

#### Scenario: A verified external publication leaves a durable receipt

- **WHEN** Homebrew or AUR publication succeeds after its remote branch is
  verified
- **THEN** the publisher uploads a per-target receipt tied to the signed
  manifest and both workflow run identities, recording the target outcome and
  exact old/new/observed commit OIDs and each published path/blob digest
  without deploy credentials

#### Scenario: An external SSH host identity changes

- **WHEN** a publication endpoint presents an unknown or changed SSH host key
- **THEN** publication fails closed before modifying the external repository

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
