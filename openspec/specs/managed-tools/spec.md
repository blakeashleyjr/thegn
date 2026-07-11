# managed-tools Specification

## Purpose

An acquire-and-pin resolver that provisions and version-pins managed developer tools (declared under [managed_tools.<name>]) for use inside worktrees.

## Requirements

### Requirement: A managed tool is described by a declarative spec

thegn SHALL represent each externally-acquired tool as a declarative
`ManagedTool` spec carrying a stable name, an acquisition source, a pinned
version, an update policy, and PATH fallback command names. The spec MUST be
substrate-agnostic domain data in `thegn-core` (no HTTP client, no tokio, no
side effects), so its resolution logic is unit-testable and coverage-gated.

The source MUST be one of: a GitHub release (repository plus a per-platform,
per-architecture asset selector) or an npm package. The update policy MUST be
one of `always`, `once`, or `never`.

#### Scenario: Spec round-trips as pure data

- **WHEN** a `ManagedTool` spec is constructed for a GitHub-release or npm tool
- **THEN** it exposes its name, source, pinned version, update policy, and PATH
  fallback names without performing any I/O

#### Scenario: GitHub-release source selects an asset per platform

- **WHEN** a GitHub-release tool spec is asked for its asset on a given
  `(os, arch)`
- **THEN** it returns the matching asset name for that platform/architecture, or
  reports that the platform is unsupported

### Requirement: Tools resolve by a fixed three-tier order

Resolving a managed tool SHALL follow a fixed, pure decision order and return
which tier satisfied it: (1) an explicit user override (a configured binary path
and optional extra arguments), then (2) a lookup on the project shell PATH by any
of the tool's fallback command names, then (3) the managed download-and-pin
location under `~/.thegn`. The decision MUST be computable without performing
the download; the actual fetch is a separate side-effecting step.

#### Scenario: User override wins

- **WHEN** the user has configured a binary path for a tool
- **THEN** resolution selects the override tier and uses the configured path and
  arguments, without PATH lookup or download

#### Scenario: PATH fallback before download

- **WHEN** no override is configured and one of the tool's fallback command names
  is found on the project shell PATH
- **THEN** resolution selects the PATH tier and uses that binary, without
  downloading

#### Scenario: Managed location as last resort

- **WHEN** no override is configured and no fallback name is on PATH
- **THEN** resolution selects the managed tier at the deterministic
  `~/.thegn` location for that tool

### Requirement: Pinned tools install once and re-verify by policy

A managed tool SHALL record its installed version with a per-tool version marker
and be considered current only when the marker equals the pinned version. The
resolver MUST compute whether an install/refresh is required from the pinned
version, the recorded marker, and the update policy, so an already-pinned tool
skips the expensive fetch unless a refresh is forced or the policy requires a
recheck.

#### Scenario: Already-pinned tool skips reinstall

- **WHEN** the recorded version marker equals the pinned version and no refresh
  is forced
- **THEN** the resolver reports the tool as current and no download/install runs

#### Scenario: Version bump triggers refresh

- **WHEN** the pinned version differs from the recorded marker
- **THEN** the resolver reports that an install/refresh is required

### Requirement: Core decides, host fetches

The download/install side effects SHALL live in `thegn-host`, driven by the
core spec: npm-sourced tools install via an `npm` subprocess and GitHub-release
tools download the selected asset and mark it executable. The fetch MUST NOT run
on the event loop (it runs on the CLI path or `spawn_blocking`, as the managed pi
install does today) and MUST surface failures rather than silently degrading the
primary action.

#### Scenario: Fetch stays off the event loop

- **WHEN** a managed tool is installed while the compositor is running
- **THEN** the install runs off the event loop and its failure is surfaced, never
  blocking rendering

### Requirement: doctor reports managed tools

`thegn doctor` SHALL report each managed tool: which resolution tier applies,
the resolved path, and the pinned-versus-installed version state, so a user can
see whether a tool is overridden, found on PATH, or managed — and whether the
managed copy is current.

#### Scenario: doctor lists a managed tool's resolution

- **WHEN** `thegn doctor` runs with a managed tool configured
- **THEN** its output names the tool, the tier that resolves it, its path, and
  whether the managed copy matches the pinned version

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
