## ADDED Requirements

### Requirement: BugStalker is a pinned managed tool

superzej SHALL describe the BugStalker debugger (`bs`) as a `managed-tools` spec
sourced from crates.io (`cargo install bugstalker`, binary `bs`) at a pinned
version, resolved by the shared three-tier order (user override → PATH → managed
`cargo install`). `szhost debug setup` MUST ensure `bs` is installed through the
resolver, and `szhost debug path` MUST report the resolved binary and tier.

#### Scenario: setup installs the pinned bs

- **WHEN** `szhost debug setup` runs on a supported platform and `bs` is neither
  overridden nor on PATH nor already installed at the pin
- **THEN** `bs` is installed via `cargo install bugstalker --version <pin>` into
  the managed location and the version marker is recorded

#### Scenario: bs on PATH is used as-is

- **WHEN** a `bs` is already on the project PATH
- **THEN** resolution selects the PATH tier and no `cargo install` runs

### Requirement: The debugger is gated to its supported platform

Because BugStalker supports only Linux on x86-64, superzej SHALL refuse to
install or launch it elsewhere with a clear message, rather than attempting an
install that cannot work. The platform gate MUST be a pure predicate over
`(os, arch)`.

#### Scenario: Unsupported platform is refused

- **WHEN** a debug verb runs on a non-Linux or non-x86-64 host
- **THEN** superzej reports that BugStalker is unsupported on this platform and
  does not attempt an install or launch

#### Scenario: Supported platform proceeds

- **WHEN** a debug verb runs on Linux x86-64
- **THEN** the platform gate passes and the verb proceeds

### Requirement: A debug session launches a debugger for a program or pid

superzej SHALL start a BugStalker session by launching `bs` for a target
program (with optional arguments) or attaching to a pid, building the session
argv purely from the resolved binary and the target. The session MUST run in the
foreground terminal (exec-replacing the `szhost debug` process) so that, when run
inside a superzej pane, it inherits that pane's sandbox and remote placement with
no extra wiring.

#### Scenario: Launch a program under the debugger

- **WHEN** `szhost debug run <program> -- <args>` runs on a supported platform
- **THEN** it exec-replaces into `bs <program> <args>` using the resolved `bs`

#### Scenario: Attach to a running process

- **WHEN** `szhost debug attach <pid>` runs on a supported platform
- **THEN** it exec-replaces into `bs` attaching to `<pid>` using the resolved `bs`

#### Scenario: A session inherits its pane's placement

- **WHEN** a debug session is started inside a pane bound to a remote-placed or
  sandboxed worktree
- **THEN** `bs` runs within that pane's sandbox/placement without the debug verb
  performing any additional sandbox or remote wrapping
