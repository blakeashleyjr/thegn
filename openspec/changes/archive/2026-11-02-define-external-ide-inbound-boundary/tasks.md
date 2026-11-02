# Tasks — external IDE inbound boundary (THE-104)

- [x] Audit current docs, schemas, binaries, packaging, and editor actions for
      any accidental inbound claims.
      Evidence: no inbound claims were found; the two `thegn://` uses are MCP
      resource names and the emitted pairing payload.
- [x] Write the no-inbound / paired-client / strict-URL decision matrix with
      security, portability, packaging, and maintenance costs.
      Evidence: design.md preserves the no-inbound / paired-client / strict-URL
      matrix and its comparison dimensions.
- [x] Decide whether inbound IDE integration is an alpha product surface.
      Evidence: the 2026-10-20 owner decision chooses deferral and publication
      of the outbound-only boundary.
- [x] Publish the chosen support state and exact outbound-versus-inbound terms.
      Evidence: docs/help/terminal-and-panes.md and
      docs/extending/ide-integration.md publish the support state and terms.
- [x] Remove any stale `thegn://`, launcher, or extension claims if unsupported.
      Evidence: no unsupported inbound claims existed in shipped public text;
      the contract test pins their absence.
- [x] If adopted, create a separate implementation OpenSpec change with catalog,
      auth, protocol, platform, tests, and documentation acceptance criteria.
      Evidence: inbound was not adopted, so no implementation change was
      filed, per the owner decision.
- [x] Add contract tests that public schemas/help cannot imply an unimplemented
      inbound door.
      Evidence: `editor_handoff_help_publishes_the_outbound_only_boundary` and
      `public_help_and_api_schema_claim_no_inbound_ide_door` cover the claims.
- [x] Run strict OpenSpec and documentation/schema gates.
      Evidence: the batch-81 gates are `just openspec-validate` and
      `python3 scripts/delivery_state.py validate`.
