# ADR-0001: Defer direct adoption of rustix

- Status: Deferred
- Date: 2026-08-29
- Scope: Unix pane, PTY, signal, file-descriptor, and resource-limit code

## Context

The host Unix seam already uses `nix` owned file descriptors and process
operations (`crates/thegn-host/src/platform/unix.rs:7-20,103-151,176-210`)
and direct `libc` for panic-safe termios and rlimits
(`crates/thegn-host/src/platform/unix.rs:37-78`,
`crates/thegn-host/src/fd_limit.rs:17-40`). PTY I/O belongs to
`portable-pty` and an off-thread blocking reader
(`crates/thegn-host/src/pane.rs:1-13`). The target-gated manifests own these
platform dependencies (`crates/thegn-host/Cargo.toml:132-140`,
`crates/thegn-core/Cargo.toml:78-93`, and
`crates/thegn-metrics/Cargo.toml:19-36`). `rustix` 1.1.4 is already
transitive in `Cargo.lock:7161-7172`, but is not a direct workspace dependency.

## Decision

Defer a broad `nix`/`libc` migration. `rustix` would add a second syscall
vocabulary without removing `portable-pty` or the libc-only macOS libproc,
Mach, and dynamic-symbol seams (`crates/thegn-core/src/activity.rs:664-823`,
`crates/thegn-metrics/src/thermal.rs:110-166`). It therefore adds maintenance
and review surface with no user-visible or meaningful binary-size gain. MSRV
1.89 is sufficient; the musl and mingw targets do not justify a Unix-only
replacement.

This is a replacement candidate, not an additional dependency. No source or
manifest change is part of this decision.

## Reopen condition

For a new Unix syscall that `nix` cannot provide, evaluate `rustix` behind the
existing `platform/` seam with a measured benefit and the musl/macOS checks.
Do not migrate existing call sites as cleanup or move syscall concerns into
`thegn-core`.

## Audit

Any future direct dependency must pass `deny.toml:8-39,41-63,70-78,94-97` and
`just deps-audit` (`justfile:455-462`).
