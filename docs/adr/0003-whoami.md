# ADR-0003: Reject whoami for current identity needs

- Status: Rejected
- Date: 2026-08-29
- Scope: Hostname and session account-name lookup

## Context

Hostname resolution already degrades through Linux procfs, non-Linux sysinfo,
and `HOSTNAME`/`COMPUTERNAME` fallbacks in
`crates/thegn-core/src/util.rs:919-956`, with precedence and non-empty tests at
`:963-990`. The daemon consumes that shared helper
(`crates/thegn-host/src/daemon/mod.rs:252-260,337-349`). The only account-name
uses are `USER` for the bare-host remote default
(`crates/thegn-core/src/remote.rs:147-164`) and `USERNAME` for the Windows
`icacls` grant target (`crates/thegn-core/src/fsperm.rs:43-68`).

There is no `whoami` direct dependency in `Cargo.toml` or `Cargo.lock`.

## Decision

Reject. `whoami` would add a direct platform implementation for real-name,
language, or account metadata that the product does not consume. It would not
replace `sysinfo`, `nix`, or an existing provider seam, and it would add
binary/build and maintenance cost on the musl and mingw target matrix for no
behavioral benefit. Keeping the environment-based session identity preserves
the existing degradation behavior and adds no unsafe surface.

Linux and musl keep their current procfs/fallback hostname path and do not
gain an identity lookup. macOS continues to use the non-Linux `sysinfo`
hostname fallback, while mingw/Windows retains `USERNAME` for the existing
`icacls` seam. A new direct crate would add build and binary cost across those
target graphs without replacing a current dependency or a user-visible need.

## Reopen condition

If a daemonized path must resolve an account with no environment, file a
focused platform change. Prefer a narrow `getpwuid_r` seam with explicit
degradation over adopting a broad identity crate for unused fields. That would
be a new dependency, subject to MSRV, cross-target, and audit review.
