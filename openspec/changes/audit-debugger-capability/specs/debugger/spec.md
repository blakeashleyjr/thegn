# Debugger

## ADDED Requirements

### Requirement: Debug adapters are declared in a registry with a built-in default

thegn SHALL select the debugger to launch from an adapter registry: a
`[[debug.adapters]]` entry declares a name, run and attach argv templates,
and the platforms it supports. BugStalker (`bs`) MUST be the built-in
default entry, keeping its managed-tool resolution and pin; user-declared
adapters resolve their command from PATH or an absolute path and get no
managed install tier. `thegn debug run`/`attach` MUST accept an adapter
selection, default to the built-in, and refuse an unknown adapter naming
the known set. Template substitution and argv construction MUST remain
pure, unit-tested logic, and the built-in entry MUST produce exactly the
argv the BugStalker integration produces today.

#### Scenario: A user adapter debugs a non-Rust program

- **WHEN** config declares a `delve` adapter with run/attach templates and
  `thegn debug run --adapter delve ./svc` runs on a supported platform
- **THEN** the session exec-replaces into the adapter's substituted argv
  with `./svc` as the debugee

#### Scenario: The default is unchanged

- **WHEN** `thegn debug run <program>` runs with no adapter flag
- **THEN** the built-in BugStalker entry is selected and the argv matches
  the pre-registry behaviour exactly

#### Scenario: An unknown adapter is refused

- **WHEN** `thegn debug run --adapter nope <program>` runs
- **THEN** the verb refuses, listing the known adapter names

### Requirement: Doctor reports the debugger's resolution and platform gate

`thegn doctor` SHALL report the debugger state: the built-in tool's
resolution tier (override / PATH / managed), resolved path, and
pinned-vs-installed currency; a note explaining the platform gate on hosts
where the built-in cannot run; and one row per configured adapter with its
resolution state and supported platforms. The probe MUST be
detection-only — doctor MUST NOT launch a debugger.

#### Scenario: An unsupported host explains the gate

- **WHEN** `thegn doctor` runs on a host outside the built-in adapter's
  platforms
- **THEN** the debugger row carries a note naming the platform restriction,
  so "not installed" is not misread as "run setup"

#### Scenario: Configured adapters are listed

- **WHEN** config declares an `lldb` adapter and `thegn doctor` runs
- **THEN** the report includes an `lldb` row with its resolved-or-missing
  command and platform list, and no debugger process is started

### Requirement: Adapter commands resolve only from trusted config layers

Because an adapter entry is subprocess argv, thegn MUST NOT execute
`[[debug.adapters]]` commands sourced from a worktree-local config layer:
until config trust resolution lands, worktree-layer entries SHALL be
ignored with a notice, and once it lands the trust decision governs.

#### Scenario: A worktree-local adapter is not used

- **WHEN** a checked-out worktree carries a config file declaring a
  `[[debug.adapters]]` entry and a debug verb runs
- **THEN** the entry is ignored with a notice and only trusted-layer
  adapters are selectable

## MODIFIED Requirements

### Requirement: The debugger is gated to its supported platform

thegn SHALL gate each adapter to the platforms it declares and refuse to
install or launch it elsewhere with a clear message naming the adapter and
its supported platforms, rather than attempting a session that cannot
work. The gate MUST be a pure predicate over the adapter's platform list
and the host `(os, arch)`. The built-in BugStalker entry declares Linux
x86-64 only.

#### Scenario: Unsupported platform is refused per adapter

- **WHEN** a debug verb selects the built-in adapter on a non-Linux or
  non-x86-64 host
- **THEN** thegn reports that BugStalker supports only Linux x86-64 and does
  not attempt an install or launch

#### Scenario: Another adapter's platforms are honored

- **WHEN** a configured adapter declares darwin support and a debug verb
  selects it on a darwin host
- **THEN** the gate passes for that adapter even though the built-in would
  be refused there

### Requirement: A debug session launches a debugger for a program or pid

thegn SHALL start a debug session by launching the selected adapter for a
target program (with optional arguments passed through to the debugee) or
attaching to a pid, building the session argv purely from the adapter's
template, the resolved binary, and the target. The session MUST run in the
foreground terminal (exec-replacing the `thegn debug` process) so that,
when run inside a thegn pane, it inherits that pane's sandbox and remote
placement with no extra wiring.

#### Scenario: Launch a program under the default debugger

- **WHEN** `thegn debug run <program> -- <args>` runs on a supported
  platform
- **THEN** it exec-replaces into the built-in adapter's run argv
  (`bs <program> <args>`) using the resolved binary

#### Scenario: Attach to a running process

- **WHEN** `thegn debug attach <pid>` runs on a supported platform
- **THEN** it exec-replaces into the selected adapter's attach argv
  (built-in: `bs -p <pid>`)

#### Scenario: A session inherits its pane's placement

- **WHEN** a debug session is started inside a pane bound to a remote-placed
  or sandboxed worktree
- **THEN** the debugger runs within that pane's sandbox/placement without
  the debug verb performing any additional sandbox or remote wrapping
