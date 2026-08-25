# macos-app-launcher

## ADDED Requirements

### Requirement: Batteries provisioning at install time

`install.sh --batteries` and `just macos-app --batteries` SHALL ensure a
full-fidelity emulator and the profile's Nerd Font are present before
generating the launcher: detecting what already exists, provisioning missing
pieces through an available package manager (brew casks or a nix-darwin
package) only with explicit user confirmation, and otherwise printing the
exact packages required and exiting nonzero without registering a launcher.
The generated bundle SHALL pin the provisioned emulator and launch it with
thegn's bundled profile, and SHALL remain locally generated — the standing
Gatekeeper rule that a distributed bundle must be signed and notarized is
unchanged.

#### Scenario: Provisioning via brew

- **WHEN** `install.sh --batteries` runs on a Mac with brew and no
  acceptable emulator or Nerd Font
- **THEN** it installs the named emulator and font casks after confirmation,
  generates the `.app` pinned to that emulator with the bundled profile, and
  prints a per-item provisioned/present summary

#### Scenario: No package manager available

- **WHEN** `--batteries` runs with neither brew nor nix available and pieces
  are missing
- **THEN** it names each missing piece and the command that provides it,
  exits nonzero, and writes no launcher artifacts

#### Scenario: Batteries dry run

- **WHEN** `install.sh --batteries --dry-run` is invoked
- **THEN** it prints the detection results and the provisioning plan and
  changes no files
