# Design — external IDE inbound boundary

## Current contract

`editor.open` is outbound only. It does not imply `thegn://`, desktop-file or
Info.plist registration, an IDE extension, a reverse reveal RPC, or a bespoke
socket. Documentation and schemas must say so explicitly.

## Decision matrix

The review must compare:

1. **No inbound product:** smallest attack/support surface.
2. **Paired thin client:** an extension uses standard pairing, catalog verbs,
   scopes, and transports; no IDE-specific auth or socket.
3. **Strict URL handler:** OS association invokes a CLI parser limited to known
   registered repos and allowlisted parameters, with no command execution or
   token redemption.

For each option record macOS/Linux/Windows ownership, headless behavior, repo
and worktree disambiguation, path canonicalization, line/column bounds, user
confirmation, replay/expiry, error reporting, packaging, and uninstall.

## Adoption gate

An adopted option requires a new implementation change that names catalog
operations, scopes, wire/schema additions, platform registrations, lifecycle,
tests, docs, and rollback. Until that change lands, inbound remains unsupported.
