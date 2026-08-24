# Managed Tools

## ADDED Requirements

### Requirement: Managed artifacts support a direct-URL file source

The managed-tool resolver SHALL support a `UrlArtifact` acquisition source in
addition to `GithubRelease`, `Npm`, and `Cargo`: a non-executable file fetched
from a direct HTTPS URL, described purely in core by its URL, a declared size
in bytes, and a pinned SHA256 checksum. As with the other sources, core
describes and decides without I/O and the host performs the fetch off the
event loop. The host MUST refuse the download unless a `download_file`
capability grant covers the URL, MUST surface the declared size before
fetching and require explicit confirmation above a size-warning threshold,
and MUST verify the SHA256 before moving the file into its managed location
and recording the version marker — a mismatch discards the download and
installs nothing. An artifact source has no PATH-lookup tier (artifacts are
not commands) and is never marked executable.

#### Scenario: Artifact resolves as pure data

- **WHEN** a `UrlArtifact` spec is constructed
- **THEN** it exposes its URL, declared size, checksum, and managed
  destination without performing any I/O, and reports no release asset and no
  PATH fallback names

#### Scenario: Grant-gated, size-warned fetch

- **WHEN** the host is asked to install a `UrlArtifact` whose declared size
  exceeds the warning threshold
- **THEN** the fetch proceeds only if a `download_file` grant matches the URL
  and the user explicitly confirmed the shown size

#### Scenario: Checksum verification gates the install

- **WHEN** the downloaded bytes hash to a SHA256 other than the pinned value
- **THEN** the download is discarded, no version marker is written, and the
  failure is surfaced

#### Scenario: doctor reports artifact state

- **WHEN** `thegn doctor` runs with a managed artifact declared
- **THEN** its output names the artifact, whether an override or the managed
  tier resolves it, and whether the installed copy matches the pinned checksum
