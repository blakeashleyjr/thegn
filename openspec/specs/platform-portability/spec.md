# platform-portability Specification

## Purpose

thegn targets Linux, macOS and Windows and every terminal from kitty to a bare `xterm`. This spec fixes where platform-conditional code may live, which crates may depend on which substrates, and which portability checks (terminal-capability matrix, feature matrix, MSRV, cross-compiles) run in CI.

## Requirements

### Requirement: Platform-conditional code lives in platform modules

`#[cfg(unix|windows|target_os|target_family)]` SHALL appear only in `platform/` modules, `termcaps`, and the `sandbox*` tables, or in files pinned by `test/platform-cfg-ratchet.txt` (shrink-only).

#### Scenario: A cfg leaks into the loop

- **WHEN** `run.rs` gains a `#[cfg(windows)]` block
- **THEN** the platform-cfg ratchet fails naming `run.rs`

### Requirement: The terminal capability matrix is CI-gated

`just term-check` (the kitty / bare xterm / `NO_COLOR` / 256color / glyph-override / color-override matrix over `thegn doctor`) SHALL run in `just ci` and as a CI job.

#### Scenario: Degradation regresses

- **WHEN** `NO_COLOR=1 thegn doctor` resolves to anything but mono
- **THEN** the term-check job fails

### Requirement: Features and MSRV build

`just check-features` SHALL type-check the workspace with `--all-features` and each named feature (`control-grpc`, `test-utils`, `profiling`, `dev`, `standalone`) individually, and `just check-msrv` SHALL type-check the workspace on the declared `rust-version`; both run in `just ci` and CI.

#### Scenario: A feature rots

- **WHEN** code behind `control-grpc` no longer compiles
- **THEN** `check-features` fails

### Requirement: The local gate is green-able

`just ci` SHALL contain only gates that pass on a clean checkout; the e2e suite SHALL live in `just ci-local` until its baselines are current.

#### Scenario: Pre-PR gate

- **WHEN** a contributor runs `just ci` on a clean checkout
- **THEN** it passes without running muse e2e
