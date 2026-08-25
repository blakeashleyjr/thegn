# Distribution

## ADDED Requirements

### Requirement: Batteries editions compose upstream terminals, never embed one

A batteries-included edition SHALL be a composition of a pinned upstream
terminal emulator, a pinned Nerd Font, thegn's bundled terminal profile, and
the thegn binary with its runtime tools. thegn SHALL NOT fork, vendor, or
embed a terminal emulator, and a batteries edition SHALL NOT introduce any
render backend beside the existing compositor. The only thegn-owned
artifacts in a batteries edition are the bundled profiles, wrappers, and
pins; emulator and font security updates flow from bumping the upstream pin,
not from a thegn release.

#### Scenario: Emulator CVE response

- **WHEN** the pinned emulator publishes a security fix
- **THEN** updating the pin (flake.lock / cask version) delivers it, with no
  change to any thegn source or release artifact

#### Scenario: No embedded emulator

- **WHEN** the batteries edition's contents are enumerated
- **THEN** the emulator and font are upstream packages referenced by pin,
  and no emulator source or binary is vendored into this repository

### Requirement: The nix batteries output is the reference edition

The flake SHALL expose a `batteries` output composing a pinned emulator, the
Nerd Font named by the bundled profiles (scoped to the launch via
fontconfig, not installed user-wide), the bundled profile, and the wrapped
stable-channel `thegn` with its pinned runtime tools. Launching it on a host
with no preinstalled terminal, font, or runtime tool SHALL yield a fully
working session. The profile the font picker patches SHALL be a writable
per-user copy, since the store copy is immutable.

#### Scenario: Clean-host launch

- **WHEN** `.#batteries` is run on a host with no terminal emulator, no Nerd
  Font, and none of thegn's runtime tools installed
- **THEN** a terminal window opens running thegn with every `thegn doctor`
  runtime-tool probe green

#### Scenario: Font switch from the batteries edition

- **WHEN** the user invokes the font picker from a batteries launch
- **THEN** it patches the writable per-user profile copy the wrapper
  launched with, and the next launch reflects the change

### Requirement: A batteries launch guarantees full-fidelity terminal capabilities

Launching any batteries edition SHALL place thegn in an environment where
capability detection resolves truecolor, full Unicode glyphs, and undercurl,
where the configured Nerd Font is present, and where Alt-based chords
deliver (option-as-alt on macOS via the bundled profile). A missing piece at
provisioning time SHALL be reported as a provisioning failure, never left as
a silent degradation.

#### Scenario: Detection from a batteries window

- **WHEN** `thegn doctor` runs inside a batteries-launched session
- **THEN** it reports truecolor color depth, full glyph level, and undercurl
  support

#### Scenario: Font missing at provisioning

- **WHEN** a batteries install cannot provide the Nerd Font
- **THEN** the installer names the missing font and the command that fixes
  it and does not report the batteries install as complete

### Requirement: Batteries platform scope is explicit and staged

Batteries editions SHALL ship the stable channel only. The supported paths
are the nix `batteries` output (Linux and macOS) and install-time
provisioning via `install.sh --batteries`; each SHALL appear in the install
matrix only after a recorded clean-host rehearsal, per the
verified-before-advertised rule. Deferred forms SHALL be tracked with entry
criteria, not instructions: a Windows batteries edition behind a green
windows-msvc release leg (its shape — a Windows Terminal profile plus a
winget dependency story — is decided inside the windows track), and portable
Linux formats (flatpak, AppImage, `nix bundle`) behind the GL/driver
portability question and actual demand. No downloadable macOS app bundle
SHALL ship unless signed and notarized.

#### Scenario: No dev-channel batteries

- **WHEN** batteries outputs and installers are enumerated
- **THEN** every one launches the stable-channel binary and none packages
  `thegn-dev`

#### Scenario: Deferred forms stay undocumented

- **WHEN** the windows-msvc release leg has not produced a verified asset
- **THEN** the install matrix carries no Windows batteries instructions, and
  the deferral records its entry criterion
