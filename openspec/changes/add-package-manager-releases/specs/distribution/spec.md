# Distribution

## ADDED Requirements

### Requirement: Release artifacts follow a stable resolution contract

Every tagged release SHALL attach, for each target whose CI job is green, an
archive named `thegn-<tag>-<target>.tar.gz` (`.zip` for windows targets)
containing the `thegn` binary at the archive root beside `LICENSE-MIT`,
`LICENSE-APACHE` and `README.md`, plus a `thegn-<tag>-<target>.sha256`
checksum asset. This naming and layout is a public contract parsed by every
downstream resolver — the staged `[package.metadata.binstall]` pkg-url, the
Homebrew formula, the AUR PKGBUILD, and convention-based installers
(`ubi`/`mise`) — and a change to it MUST update all generated manifests and
the binstall metadata in the same change.

#### Scenario: Tag push produces the contracted assets

- **WHEN** a `v*` tag is pushed
- **THEN** each target in the release matrix uploads an archive and a
  checksum whose names match the contract, with the binary at the archive
  root

#### Scenario: Renaming is treated as breaking

- **WHEN** a change alters the archive name pattern or root layout
- **THEN** the same change updates the Homebrew template, the AUR template
  and `[package.metadata.binstall]`, and the release notes call out the
  break for convention-based installers

### Requirement: Release artifacts carry build provenance

Release CI SHALL attach keyless build-provenance attestations (GitHub OIDC
via `actions/attest-build-provenance`) to every uploaded archive, so that
`gh attestation verify <archive> -R <owner>/thegn` succeeds for a genuine
asset. Artifact integrity SHALL NOT depend on any long-lived signing key
held as a CI secret. macOS artifacts remain unsigned and un-notarized per
the standing RELEASING.md decision; its revisit criteria are unchanged.

#### Scenario: Verifying a downloaded archive

- **WHEN** a user downloads a release archive and runs
  `gh attestation verify` against it
- **THEN** verification succeeds and names this repository's release
  workflow as the builder

#### Scenario: No signing key custody

- **WHEN** the release pipeline's secrets are enumerated
- **THEN** no artifact-signing private key is among them — only
  publishing-surface credentials (tap deploy key, AUR key)

### Requirement: Package-manager manifests are rendered by release CI

For each automated manager (the Homebrew tap formula and the AUR
`thegn-bin` package), release CI SHALL render the version-pinned manifest
from its in-repo template (`packaging/homebrew/`, `packaging/aur/`),
substituting the tag's version and the checksums published by the same
tag's workflow — never hand-pasted values — and push it to the manager's
publishing surface with a credential scoped to that surface alone. The
`.SRCINFO` accompanying the PKGBUILD SHALL be generated, not hand-edited. A
manifest publish failure SHALL fail its own job without retracting or
modifying release assets, and re-running the job SHALL be idempotent.

#### Scenario: Homebrew formula bump

- **WHEN** a release's assets and checksums are published for a tag
- **THEN** the tap repo receives a commit updating `Formula/thegn.rb` to
  that version with the darwin archive's sha256, and
  `brew install <owner>/tap/thegn` resolves the new release

#### Scenario: AUR update

- **WHEN** the same trigger fires
- **THEN** the `thegn-bin` AUR package receives a PKGBUILD + regenerated
  `.SRCINFO` pointing at the linux-musl asset with its sha256

#### Scenario: Manifest failure never touches assets

- **WHEN** a manifest push fails (auth, network, upstream conflict)
- **THEN** the release assets, checksums and attestations are unaffected,
  and re-running the failed job publishes the identical manifest

### Requirement: Package managers ship the stable channel

Every artifact published to a package manager SHALL be the stable-channel
binary under the stable names (`thegn`/`tg`). The dev channel remains a
nix (`.#dev`) or from-source install, and `THEGN_CHANNEL` remains the
runtime override on any binary.

#### Scenario: No dev-channel manifests

- **WHEN** the manifests job runs
- **THEN** it publishes no `thegn-dev` formula, package or manifest

#### Scenario: Dev channel on a packaged binary

- **WHEN** a user runs a package-manager-installed `thegn` with
  `THEGN_CHANNEL=dev`
- **THEN** the channel resolution honors the override exactly as for any
  other install path

### Requirement: A package manager is advertised only when verified

The documented install matrix (README and help pages) SHALL list a package
manager only after its end-to-end install path has been exercised by CI or
a recorded release rehearsal. Managers with unmet gates SHALL be tracked as
deferred with explicit entry criteria — crates.io publishing behind the
workspace publish decision; scoop/winget behind a green windows-msvc
release leg; hosted apt/rpm repositories and homebrew-core/nixpkgs upstream
behind post-1.0 maintainership decisions — and SHALL NOT appear as install
instructions.

#### Scenario: Staged manager stays undocumented

- **WHEN** the windows-msvc release leg has not produced a verified asset
- **THEN** the install matrix carries no scoop or winget instructions

#### Scenario: Convention installers are documented once proven

- **WHEN** an `ubi`/`mise` one-liner has been verified against a real
  release's assets
- **THEN** the install matrix may list it, since it costs nothing per
  release thereafter
