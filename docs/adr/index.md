# Dependency adoption decisions

These records answer THE-61 against the checked-out workspace. They describe
the current dependency boundary and the conditions for reopening a decision;
they do not authorize a runtime migration.

| Candidate                               | Decision           | Record                                |
| --------------------------------------- | ------------------ | ------------------------------------- |
| `rustix`                                | Defer              | [ADR-0001](0001-rustix.md)            |
| `windows-rs` (`windows`, `windows-sys`) | Adopt / keep split | [ADR-0002](0002-windows-rs.md)        |
| `whoami`                                | Reject             | [ADR-0003](0003-whoami.md)            |
| `sysinfo`                               | Adopt / keep       | [ADR-0004](0004-sysinfo.md)           |
| `zerocopy`                              | Defer              | [ADR-0005](0005-zerocopy.md)          |
| `tokio-tungstenite` / `tungstenite`     | Adopt / keep       | [ADR-0006](0006-tokio-tungstenite.md) |

## Shared review constraints

The workspace MSRV is 1.89 (`Cargo.toml:29-33`). Target-sensitive decisions
must preserve the Linux/musl bridge and the macOS and mingw lanes described by
`justfile:93-143` and `flake.nix:179-189,226-255`. Dependency changes are
reviewed with `just deps-audit` (`justfile:455-462`), whose CI job is defined
at `.github/workflows/ci.yml:121-138`; `just lint` is a separate gate on this
branch. See the individual records for usage, ownership, and migration
conditions.
