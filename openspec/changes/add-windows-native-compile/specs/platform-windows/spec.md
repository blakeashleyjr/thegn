# Platform: native Windows

## ADDED Requirements

### Requirement: The workspace compiles for Windows targets

The cargo workspace SHALL compile (`cargo check --workspace`) for
`x86_64-pc-windows-gnu` and `x86_64-pc-windows-msvc` with no unix behavior
change. Unix-only dependencies (`nix`, `libc`) MUST be target-gated, and
OS-conditional syscalls MUST live behind the host `platform` seam
(`crates/thegn-host/src/platform/`) rather than inline `#[cfg]` blocks at call
sites.

#### Scenario: Linux-side cross-check gates regressions

- **WHEN** `just check-cross` runs on a PR
- **THEN** `cargo check --workspace --target x86_64-pc-windows-gnu` passes,
  catching any newly introduced ungated unix API use

#### Scenario: msvc truth gate

- **WHEN** the opt-in `windows` CI job runs (dispatch or `[ci-windows]` marker)
- **THEN** `cargo check --workspace --locked` passes on `windows-latest` with a
  bare rustup toolchain (no nix)

### Requirement: Local shell invocations use the platform shell dialect

Argvs that hand a command string to the **local** user shell (pins, tool
drawer, custom actions, pane-run, editor-open) SHALL be built by
`thegn_core::shellinv`, which maps POSIX shells to `-c`/`-lc`, PowerShell to
`-NoProfile -Command`, and cmd.exe to `/C`. Call sites targeting a remote or
sandboxed Linux substrate SHALL keep literal `sh -lc` and carry an annotation
saying so.

#### Scenario: A pin command on Windows

- **WHEN** a pin with a bare `command` launches on a host whose shell resolves
  to `pwsh.exe`
- **THEN** the spawn argv is `[pwsh.exe, -NoProfile, -Command, <command>]`
  (no `-lc`, no `exec` prefix)

#### Scenario: A pin command on unix is unchanged

- **WHEN** the same pin launches on a unix host
- **THEN** the argv is `[$SHELL, -lc, exec <command>]` exactly as before

### Requirement: Unix-substrate features stub with explicit errors on Windows

Features whose substrate is inherently unix in this phase — the pane daemon and
control client (unix-socket IPC), the sealed-sandbox model relay, the
merge-queue headless agent, the SIGUSR2 profiler, `thegn debug` exec-replace —
SHALL return an explicit error (or logged warning, for best-effort paths) on
Windows rather than silently no-op or panic. The bare `thegn` compositor MUST
still start with the daemon stubbed.

#### Scenario: Daemon subcommand on Windows

- **WHEN** `thegn daemon` runs on native Windows (phase 1)
- **THEN** it exits with an error naming unix-socket IPC as the unsupported
  piece rather than crashing or hanging

### Requirement: Process control routes through the platform seam

Pid liveness probes, best-effort termination, process-group creation and
tree kills, stderr redirection, and shutdown-signal installation SHALL go
through `thegn-host`'s `platform` module. On unix these keep today's exact
semantics (SIGTERM, pgids, dup2); on Windows they map to `OpenProcess`/
`GetExitCodeProcess`, `TerminateProcess`, per-child kill (Job Objects arrive in
a later change), `SetStdHandle`, and console ctrl events respectively.

#### Scenario: Compositor shutdown signal on Windows

- **WHEN** the console window closes or Ctrl+C is delivered to the compositor
- **THEN** the shutdown flag is set and the terminal waker is pulsed — the same
  contract as SIGTERM/SIGHUP on unix — so session state persists before exit
