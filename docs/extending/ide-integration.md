# IDE integration boundary

Outbound `editor.open` is the supported IDE handoff contract. A request names a
registered worktree and may include a contained relative file with optional
line and column. The request is validated and queued, then launched off the UI
loop through the configured `[editor] provider`.

Inbound integration is not supported in this release. This includes a URL
scheme, an OS association, an IDE extension, a reverse reveal RPC, or an
IDE-specific socket. Any future proposal must first satisfy the adoption gate.
An adopted option requires a new implementation change that names catalog
operations, scopes, wire/schema additions, platform registrations, lifecycle,
tests, docs, and rollback. Until that change lands, inbound remains unsupported.

The gate: `ide_handoff::tests::editor_handoff_help_publishes_the_outbound_only_boundary`
and `ide_handoff::tests::public_help_and_api_schema_claim_no_inbound_ide_door`.
