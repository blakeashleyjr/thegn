# IDE integration boundary

Outbound `editor.open` is the supported IDE handoff contract. A request names a
registered worktree and may include a contained relative file with optional
line and column. The request is validated and queued, then launched off the UI
loop through the configured `[editor] provider`.

Inbound integration is not supported in this release. This includes a URL
scheme, an OS association, an IDE extension, a reverse reveal RPC, or an
IDE-specific socket. Any future proposal must first satisfy the adoption gate:

- Name the catalog operations, scopes, wire and schema additions, and lifecycle.
- Define platform ownership and registrations, including headless behavior,
  packaging, uninstall, and rollback.
- Specify repository and worktree resolution, canonical path confinement, and
  parameter bounds.
- Define authentication, user confirmation, expiry and replay behavior, and
  error reporting.
- Add contract tests and documentation for the implemented surface.

The gate: `ide_handoff::tests::editor_handoff_help_publishes_the_outbound_only_boundary`
and `ide_handoff::tests::public_help_and_api_schema_claim_no_inbound_ide_door`.
