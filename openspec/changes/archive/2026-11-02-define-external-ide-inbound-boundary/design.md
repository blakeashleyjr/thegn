# Design — external IDE inbound boundary

## Current contract

`editor.open` is outbound only. It does not imply `thegn://`, desktop-file or
Info.plist registration, an IDE extension, a reverse reveal RPC, or a bespoke
socket. Documentation and schemas must say so explicitly.

## Decision matrix

The review must compare:

1. **No inbound product:** smallest attack/support surface.
2. **CLI-only inbound:** `thegn open <worktree> [file:line:col]` resolves through
   the local control socket.
3. **Full URL and extension:** a `thegn://` scheme has desktop/URL registrations
   and IDE extension fixtures.

For each option record macOS/Linux/Windows ownership, headless behavior, repo
and worktree disambiguation, path canonicalization, line/column bounds, user
confirmation, replay/expiry, error reporting, packaging, and uninstall.

## Adoption gate

An adopted option requires a separate implementation change that:

- Names catalog operations, scopes, wire/schema additions, and lifecycle.
- Defines platform ownership and registrations, including headless behavior,
  packaging, uninstall, and rollback.
- Specifies repository and worktree resolution, canonical path confinement, and
  parameter bounds.
- Defines authentication, user confirmation, expiry and replay behavior, and
  error reporting.
- Adds contract tests and documentation for the implemented surface.

Until that change lands, inbound remains unsupported.

## Decision (2026-10-20)

The options considered were:

- **A. Defer:** publish an outbound-only boundary (TUI, control API, MCP), remove
  inbound-scheme wording, and file nothing new.
- **B. CLI-only inbound:** add `thegn open <worktree> [file:line:col]` over the
  local control socket.
- **C. Full URL and extension:** add a `thegn://` scheme, desktop/URL
  registrations, and IDE extension fixtures.

Choose **A**. There is no named inbound client requiring this product surface,
and every registered scheme adds attack surface and ongoing platform support.
The owner decision on 2026-10-20 was to defer inbound integration and publish
an outbound-only boundary.
