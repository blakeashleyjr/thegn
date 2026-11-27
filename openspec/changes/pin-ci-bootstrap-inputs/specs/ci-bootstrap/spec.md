# CI and contributor bootstrap authenticity

## Requirements

### Requirement: Authenticate CI bootstrap executables

CI setup MUST use repository-pinned version, exact size, and SHA-256 identities
for the Nix installer and Actions runner archive. Downloads MUST be bounded,
staged privately, and authenticated before execution, extraction, or promotion.
Cached runner archives MUST be re-authenticated on every use. Runner archive
members MUST be checked for safe paths, link containment, allowed file types,
duplicate destinations, member-count limits, and expanded-size limits before
extraction.

#### Scenario: Pinned CI artifacts are authentic

- **WHEN** setup downloads the Nix installer or Actions runner release
- **THEN** it selects the pinned release and verifies exact size and SHA-256
- **AND** it executes or installs the artifact only after verification succeeds

#### Scenario: Cached runner archive has changed

- **WHEN** a cached runner archive differs from its pinned size or digest
- **THEN** setup refuses to extract or install it
- **AND** setup does not treat the cache's existence as evidence of integrity

#### Scenario: Runner archive is unsafe

- **WHEN** an authenticated archive has unsafe paths, escaping links,
  unsupported members, duplicate destinations, or exceeds archive limits
- **THEN** setup refuses before extracting any member

### Requirement: Authenticate the macOS rustup bootstrap

The bare macOS contributor setup MUST select a versioned rustup-init binary
whose exact size and SHA-256 identity are stored in repository-reviewed source
for each supported Mac architecture. It MUST bound and privately stage the
download, then verify its digest before executing it.

#### Scenario: Pinned rustup-init is authentic

- **WHEN** a contributor accepts rustup installation on supported macOS
- **THEN** setup downloads the architecture-specific pinned rustup-init
- **AND** executes it only after size and digest verification succeed

#### Scenario: rustup-init cannot be authenticated

- **WHEN** download fails, exceeds its bound, or the digest differs
- **THEN** setup refuses before running rustup-init
- **AND** removes the private staging directory
