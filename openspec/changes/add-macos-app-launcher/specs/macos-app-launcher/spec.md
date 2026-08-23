## ADDED Requirements

### Requirement: Platform-appropriate launcher registration

The installer SHALL register thegn with whatever launcher registry the host platform has, detected
at install time rather than assumed. On Linux/BSD that is a freedesktop `.desktop` entry plus an
hicolor icon; on macOS it is a generated `thegn.app` bundle; on platforms with neither, the
installer SHALL install binaries only and SHALL NOT report launcher files it did not write.

#### Scenario: Installing on macOS

- **WHEN** `install.sh` runs on a host whose `uname -s` is `Darwin`
- **THEN** it generates a `thegn.app` bundle in `~/Applications` pointed at the installed binary
- **AND** it writes no `.desktop` entry and no hicolor icon
- **AND** its summary names the app bundle and neither of the freedesktop paths

#### Scenario: Installing on Linux or BSD

- **WHEN** `install.sh` runs on a host with a freedesktop launcher
- **THEN** it writes the `.desktop` entry and hicolor icon as before
- **AND** it generates no macOS app bundle

#### Scenario: Dry run

- **WHEN** `install.sh --dry-run` is invoked
- **THEN** it prints the launcher artifacts for the detected platform and changes no files

### Requirement: macOS bundle generation

thegn SHALL provide a generator (`packaging/macos/make-app.sh`) that produces a complete `.app`
bundle — `Info.plist`, bundle executable, `.icns` icon, and a Terminal.app runner — for a given
thegn binary and destination. The generator SHALL be reachable both from `install.sh` (source
installs) and from `just macos-app` (the Nix and Homebrew installs, which never run `install.sh`).
Re-running the generator SHALL overwrite an existing bundle of the same name in place.

#### Scenario: Generating for a binary on PATH

- **WHEN** `just macos-app` runs with no arguments
- **THEN** the bundle is generated against the `thegn` found on `PATH`
- **AND** the launcher is searchable by name in Spotlight, Raycast, Alfred and the Dock

#### Scenario: Version metadata

- **WHEN** the generator builds `Info.plist` for a binary reporting a pre-release version
- **THEN** `CFBundleShortVersionString` carries the full version including the pre-release suffix
- **AND** `CFBundleVersion` carries only the numeric-dotted portion, which is all macOS accepts
  there

#### Scenario: Regenerating the icon

- **WHEN** `just icons` runs
- **THEN** both `config/thegn.svg` and `packaging/macos/thegn.icns` are re-rendered from the sprite
  in `crates/thegn-host/src/owl.rs`
- **AND** the `.icns` render requires no rasterizer and no macOS-only tool, so it also runs on Linux

### Requirement: Launch-time resolution under launchd

The bundle SHALL resolve both the terminal emulator and the thegn binary by absolute path, and
SHALL run thegn through a login shell. A GUI launch inherits launchd's environment, which contains
none of the prefixes thegn installs into (nix profiles, `~/.local/bin`, Homebrew), and a macOS
terminal's CLI usually lives inside its own `.app` rather than on `PATH`; a `PATH`-based lookup
therefore finds neither.

#### Scenario: Opening the app

- **WHEN** the user launches `thegn.app`
- **THEN** the first terminal found among Ghostty, WezTerm, kitty, Alacritty and Terminal.app opens
- **AND** thegn runs inside it with the environment an interactive terminal would have, so the
  tools it shells out to (`git`, `gh`, `fzf`, `lazygit`, `delta`) are on `PATH`

#### Scenario: The recorded binary has moved

- **WHEN** the binary baked into the bundle is no longer executable (a reinstalled nix profile, a
  pruned build directory)
- **THEN** the launcher searches the known install prefixes for a `thegn` binary and uses the first
  it finds

#### Scenario: No thegn binary can be found

- **WHEN** neither the baked path nor any known prefix holds an executable thegn
- **THEN** the launcher displays a GUI alert naming the problem and the command that fixes it,
  rather than exiting silently — a GUI launch has nowhere to print

#### Scenario: No terminal emulator is installed

- **WHEN** none of the preferred emulators is present
- **THEN** the launcher falls back to Terminal.app, which every Mac has, via the bundle's `.command`
  runner

### Requirement: Locally generated bundles avoid Gatekeeper

The `.app` SHALL be generated on the user's machine rather than downloaded, so that it carries no
`com.apple.quarantine` extended attribute and opens without code signing or notarization. Any
future distribution channel SHALL either generate the bundle at install time or ship it signed with
a Developer ID and notarized.

#### Scenario: Opening a freshly generated bundle

- **WHEN** the user opens a bundle produced by the generator on their own machine
- **THEN** Gatekeeper does not block it, and no `xattr` removal or right-click-open is required

### Requirement: Environment overrides for side-by-side launchers

The generator SHALL accept repeatable `--env KEY=VALUE` arguments that are exported in the launched
session, so a second bundle under a distinct name can run a different binary with different
environment without disturbing the primary launcher.

#### Scenario: A watchable debug launcher

- **WHEN** a bundle is generated with a debug binary, `THEGN_LOG`, `THEGN_PERF` and an isolated
  `XDG_STATE_HOME`
- **THEN** launching it writes that session's log under the isolated state directory
- **AND** the primary `thegn.app`, its state, and its pane daemon are unaffected
