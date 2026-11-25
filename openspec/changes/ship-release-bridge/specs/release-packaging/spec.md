## ADDED Requirements

### Requirement: Release archives ship the resident bridge

Every unix release archive SHALL contain an executable `thegn-musl` (static
x86_64-linux-musl) at the archive root, and the Homebrew, AUR, deb and rpm packages
SHALL install it next to `thegn`. Release validation SHALL reject an archive
without it.

#### Scenario: archive missing the bridge

- **WHEN** a unix release archive lacks `thegn-musl`
- **THEN** release validation fails with a missing-root-file error
