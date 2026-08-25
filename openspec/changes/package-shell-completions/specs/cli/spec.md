# CLI

## MODIFIED Requirements

### Requirement: Shell completions are generated from the CLI definition

thegn SHALL provide `completions <shell>` generating shell completions from
the live clap definition, using the invoked binary name (thegn / tg) as
the completion target. The nix package SHALL install generated completions
for **every binary name it installs** (the channel binary and its short
alias) for bash, zsh and fish into the standard share directories, generating
each script via that binary name so the script targets the command the user
types; when the build platform cannot execute the host binary (cross builds),
the package MUST skip completion generation rather than fail. Installation
documentation MUST cover non-package installs with the static-file and
eval-on-startup patterns per shell.

#### Scenario: Bash completions

- **WHEN** `thegn completions bash` runs
- **THEN** a completion script for the invoked binary name is written to stdout

#### Scenario: The package completes the alias

- **WHEN** the nix package is built on a platform that can execute the host
  binary
- **THEN** `share/zsh/site-functions/` contains completion files for both the
  channel binary name and its alias, and the alias's script targets the alias
  (e.g. `#compdef tg`)

#### Scenario: Cross builds skip, never fail

- **WHEN** the package is built for a host platform the build platform cannot
  execute
- **THEN** the build succeeds with no completion files installed
