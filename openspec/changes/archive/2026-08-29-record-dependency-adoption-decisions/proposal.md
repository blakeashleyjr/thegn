# Record dependency adoption decisions

Linear: THE-61

## Why

THE-61 asks whether thegn should use `rustix`, `windows-rs`, `whoami`,
`sysinfo`, `zerocopy`, and `tokio-tungstenite`/`tungstenite`. The checked-out
workspace already answers three of these questions in code: `sysinfo`, the
Windows-rs split, and tungstenite are adopted. The other three are not direct
dependencies and have no current use that justifies adding them.

The missing artifact is a durable per-candidate record grounded in the actual
manifests, lockfile, call sites, target lanes, and audit gates. The records
will live in `docs/adr/`; the architect pipeline design is the implementation
plan.

## Changes

- Add an index and one ADR for each requested crate/family.
- Add one architecture-gates requirement for the existing dependency audit
  and adoption-record convention, then sync it into the canonical spec.
- Archive this completed OpenSpec change after validation.

The final decisions are: defer `rustix`; keep the existing split for
`windows`/`windows-sys`; reject `whoami`; keep `sysinfo`; defer `zerocopy`;
and keep `tokio-tungstenite`/`tungstenite` aligned with axum.

## Non-goals

This change adds no dependency, changes no version or lockfile, changes no
runtime behavior, and adds no config key, capability, help page, ratchet,
database schema, or render/wake path. In particular, the draft's proposed
Windows binding version alignment is deferred: it requires a target-tested
follow-up and is not a trivially safe adoption.

## Impact

The dependency spine is already documented in `tasks.md:232-251`; there is no
numbered roadmap item to modify. Existing Windows and macOS parity work already
uses the relevant platform seams, but does not satisfy or authorize version
alignment. The audit facts are `deny.toml:8-97`, `justfile:455-462`,
`justfile:394-397`, and `.github/workflows/ci.yml:121-138`. `just lint` is not
the dependency-audit recipe on this branch.
