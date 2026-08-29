# ADR-0002: Keep the windows-rs split at platform and media edges

- Status: Adopted / keep
- Date: 2026-08-29
- Scope: Win32 host integration and WinRT media integration

## Context

The workspace already uses both published windows-rs forms. The host platform
seam imports `windows_sys` for console, process, and Job Object APIs
(`crates/thegn-host/src/platform/windows.rs:17-27,89-143,156-241`) and owns
the target-gated declaration (`crates/thegn-host/Cargo.toml:132-140`). The
media leaf uses the higher-level `windows` WinRT SMTC API
(`crates/thegn-media/src/smtc.rs:1-25`) with only the required features
(`crates/thegn-media/Cargo.toml:37-43`). `thegn-core` does not import either
binding family.

The direct pins are `windows-sys = 0.59` and `windows = 0.58`
(`Cargo.toml:165-177`). The lock also contains newer transitive cohorts
(`Cargo.lock:9607-9628,9810-9840`), so version alignment is real maintenance
debt. It is not a safe one-line adoption: the WinRT leaf may require source
changes and Windows target validation.

## Decision

Adopt / keep the split: raw Win32 stays in `thegn-host/src/platform`, and
WinRT stays in the `thegn-media` leaf. This preserves provider seams and keeps
`thegn-core` substrate-free. Do not add `winapi` or move both concerns to one
binding crate.

Defer aligning the direct versions to a separate target-tested update. That
change must preserve feature lists, update the lock deliberately, run the
mingw and MSRV checks, and inspect `cargo tree --target all -i windows-sys` and
`-i windows`. Binary-size impact is Windows-only and mostly compile-graph
deduplication; MSRV 1.89 is not the blocker. The musl and macOS products must
remain unaffected.

## Reopen condition

Perform version alignment only as a separate migration with the Windows GNU
lane (`justfile:111-143`), the configured full Windows check when available,
and the target definitions in `flake.nix:179-189`. No alignment is included
in this records-only change.

## Audit

The known duplicate-version posture remains `multiple-versions = "warn"`
(`deny.toml:70-78`). Do not change the policy comment or ratchet until the
remaining upstream splits are handled deliberately.
