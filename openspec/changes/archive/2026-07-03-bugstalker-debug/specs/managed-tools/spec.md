## ADDED Requirements

### Requirement: Managed tools support a Cargo source

The managed-tool resolver SHALL support a `Cargo` acquisition source in addition
to `GithubRelease` and `Npm`: a crates.io crate installed with `cargo install
<crate> --version <version> --root <managed_dir>`, whose binary lands at
`<managed_dir>/bin/<name>`. As with the other sources, core describes the spec
purely (no I/O) and the host performs the `cargo install`; a `Cargo` source has
no GitHub-release asset.

#### Scenario: Cargo tool resolves to its managed bin path

- **WHEN** a `Cargo`-sourced tool falls through to the managed tier
- **THEN** its resolved path is `<managed_dir>/bin/<name>` and it reports no
  release asset for any platform

#### Scenario: Cargo tool installs via cargo

- **WHEN** a `Cargo`-sourced tool needs installation
- **THEN** the host runs `cargo install <crate> --version <version> --root
<managed_dir>` off the event loop and records the version marker on success
