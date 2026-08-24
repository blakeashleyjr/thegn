# architecture-gates Specification

## Purpose

The invariants that keep thegn portable and plugin-friendly — platform code in platform modules, colors and glyphs through the capability chokepoints, vendor CLIs behind their seam, a never-polling idle loop, deliberate ignored results — are enforced by shrink-only ratchets and lint guards rather than by convention. This spec is the ratchet mechanism and the invariant → gate mapping.

## Requirements

### Requirement: Invariants are enforced by shrink-only ratchets

Every architectural invariant the project documents SHALL have a named gate that runs in `just lint` or `just test`. A ratchet gate MUST hold a committed allowlist of the files currently violating the invariant, MUST fail when a file not in the allowlist violates it, MUST fail when an allowlisted file no longer violates it (stale entry), and MUST offer a single regeneration switch. Allowlists only shrink.

#### Scenario: New violation fails

- **WHEN** a `#[cfg(unix)]` is added to a file outside `platform/` that is not in `test/platform-cfg-ratchet.txt`
- **THEN** `just test` fails naming the file and the allowlist

#### Scenario: Paid-down debt must be recorded

- **WHEN** the last platform cfg is removed from an allowlisted file
- **THEN** the ratchet fails until that file's entry is deleted

#### Scenario: Regeneration rewrites the allowlist

- **WHEN** the ratchet runs with its update switch set
- **THEN** the allowlist is rewritten to the current violating set, preserving its header comment

### Requirement: Crate dependency boundaries are enforced

A test over every workspace member's manifest SHALL assert that `tokio`, `termwiz`, `portable-pty`, `reqwest`, `octocrab`, `axum` and `alacritty_terminal` are direct dependencies only of their declared owner crates, that `thegn-core` carries none of them, and `cargo deny` SHALL ban `vt100` and `russh` outright.

#### Scenario: Core gains a runtime dependency

- **WHEN** `tokio` is added to `crates/thegn-core/Cargo.toml`
- **THEN** `just test` fails naming the crate and the owner list

### Requirement: Render chokepoints are enforced

Color and glyph literals SHALL appear only in the chokepoint modules (`wire.rs`, `caps.rs`, `theme*`, `glyphs`, the ratatui bridge) or in files pinned by `test/color-literal-ratchet.txt` / `test/glyph-literal-ratchet.txt`.

#### Scenario: A draw site bypasses caps

- **WHEN** a new chrome file constructs a `Color::Rgb` literal
- **THEN** the color-literal ratchet fails

### Requirement: Vendor CLIs are isolated behind their seam

`thegn_core::github::` calls and `Command::new("gh")` SHALL appear only in the forge implementation files or in files pinned by `test/forge-leak-ratchet.txt`.

#### Scenario: A new module shells out to gh

- **WHEN** a file outside the forge impl adds `Command::new("gh")`
- **THEN** `just lint` fails

### Requirement: Ignored results are deliberate

The workspace SHALL enable `clippy::let_underscore_future` (deny) — a dropped future never ran — and a file-level ratchet SHALL track every file containing `let _ =` / `.ok();`, which leaves the list only when each ignore is annotated `best-effort` or handled. (`let_underscore_must_use` is deliberately not enabled: `Result` is `#[must_use]`, so it would reject the sanctioned best-effort idiom itself.)

#### Scenario: A future is dropped

- **WHEN** a `let _ = some_async_fn();` is written
- **THEN** clippy fails the build
