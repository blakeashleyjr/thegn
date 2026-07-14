# Platform: native Windows

## MODIFIED Requirements

### Requirement: The workspace compiles for Windows targets

The cargo workspace SHALL compile (`cargo check --workspace`) for
`x86_64-pc-windows-gnu` and `x86_64-pc-windows-msvc` with no unix behavior
change. Unix-only dependencies (`nix`, `libc`) MUST be target-gated, and
OS-conditional syscalls MUST live behind the host `platform` seam
(`crates/thegn-host/src/platform/`) rather than inline `#[cfg]` blocks at call
sites. The msvc gate SHALL run routinely (every push/PR, not opt-in) and
SHALL publish a release `thegn.exe` artifact
(`thegn-x86_64-pc-windows-msvc`) as the native-Windows download, alongside
the documented `cargo install --path crates/thegn-host` path.

#### Scenario: Linux-side cross-check gates regressions

- **WHEN** `just check-cross` runs on a PR
- **THEN** `cargo check --workspace --target x86_64-pc-windows-gnu` passes,
  catching any newly introduced ungated unix API use

#### Scenario: msvc truth gate is routine

- **WHEN** any push or PR triggers CI
- **THEN** the `windows` job runs `cargo check --workspace --locked` plus the
  named-pipe and Job-Object kernel tests on `windows-latest` with a bare
  rustup toolchain (no nix)

#### Scenario: Every windows run ships a binary

- **WHEN** the `windows` CI job completes
- **THEN** a release-profile `thegn.exe` is uploaded as the
  `thegn-x86_64-pc-windows-msvc` artifact
