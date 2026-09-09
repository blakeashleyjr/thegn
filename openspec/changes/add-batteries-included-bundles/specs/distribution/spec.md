# Distribution

## ADDED Requirements

### Requirement: The Nix batteries package is self-contained and user-local

The Nix batteries package SHALL compose the pinned Thegn binary, Alacritty, and
Fira Code Nerd Font, generate Fontconfig state for that font, create a writable
user-local Alacritty configuration from the immutable template, and launch
Thegn through Alacritty with `THEGN_ALACRITTY_CONFIG`. It MUST NOT replace an
existing terminal or install system-wide fonts/configuration.

#### Scenario: Nix batteries launches without host terminal setup

- **WHEN** a compatible host runs `nix run .#batteries`
- **THEN** the packaged terminal starts Thegn using the packaged font and a
  writable generated configuration

### Requirement: Supported batteries paths are explicit

Release documentation SHALL publish a matrix for Linux (Nix and non-Nix),
macOS, and Windows. For each path it MUST identify bundled, generated
user-local, and host-delegated terminal/font/config components and whether the
path is shipped, partial, or deferred. Unsupported paths MUST provide a clear
fallback rather than imply parity.

#### Scenario: An undecided Windows path is not advertised

- **WHEN** no Windows batteries artifact has passed rehearsal
- **THEN** the matrix marks it deferred, links a bounded decision/implementation
  issue, and provides fallback instructions

### Requirement: Standalone Linux has an opt-in deterministic batteries path

The standalone Linux installer SHALL expose an explicit batteries mode or
document and test a deterministic package-native equivalent. It MUST NOT
silently replace the user's default terminal or mutate system-wide font
configuration, and repeated installation SHALL have defined upgrade/uninstall
behavior.

#### Scenario: Ordinary install preserves terminal choice

- **WHEN** a user performs a standard non-batteries install
- **THEN** existing terminal/font configuration is unchanged and missing
  requirements are reported actionably

### Requirement: Claimed artifacts use verified release inputs and rehearsals

Every batteries artifact SHALL consume the checksummed/attested release inputs
owned by THE-52 and SHALL be exercised on a clean representative host/VM before
being advertised. The rehearsal record MUST name artifact identity, exact
install/launch commands, terminal/font/config diagnostics, and recovery or
uninstall behavior.

#### Scenario: A rendered artifact is not yet supported

- **WHEN** an artifact builds but has no recorded clean-host rehearsal
- **THEN** installation documentation does not advertise it as a supported
  batteries path

### Requirement: Diagnostics cover the complete launch chain

For each supported path, startup and `thegn doctor` SHALL distinguish missing
terminal, unavailable Nerd Font, unwritable generated configuration, absent
launcher component, and an invalid/unverified binary input, with path-specific
recovery guidance.

#### Scenario: Generated config is unwritable

- **WHEN** the selected batteries launcher cannot create or update its
  user-local terminal configuration
- **THEN** startup/doctor names the path and corrective action rather than
  silently falling back to an unrelated terminal configuration
