# Tasks — external IDE inbound boundary (THE-104)

- [x] Audit current docs, schemas, binaries, packaging, and editor actions for
      accidental inbound claims.
      Evidence: no inbound claims were found; the two `thegn://` uses are MCP
      resource names and the emitted pairing payload.
- [x] Write the no-inbound / CLI-only / full URL-and-extension decision matrix with
      security, portability, packaging, and maintenance costs.
      Evidence: design.md records options A/B/C, comparison dimensions, and the
      decision rationale.
- [x] Decide whether inbound IDE integration is an alpha product surface.
      Evidence: owner decision 2026-10-20 chooses defer, option A.
- [x] Publish the chosen support state and exact outbound-versus-inbound terms.
      Evidence: docs/help/terminal-and-panes.md and
      docs/extending/ide-integration.md.
- [x] Remove any stale `thegn://`, launcher, or extension claims if unsupported.
      Evidence: none existed in shipped public text; the contract test pins the
      absence of inbound claims.
- [x] If adopted, create a separate implementation OpenSpec change with
      catalog, auth, protocol, platform, tests, and documentation criteria.
      Evidence: not adopted; no implementation change filed, per the owner
      decision.
- [x] Add contract tests that public schemas/help cannot imply an unimplemented
      inbound door.
      Evidence: tests
      `ide_handoff::tests::editor_handoff_help_publishes_the_outbound_only_boundary`
      and `ide_handoff::tests::public_help_and_api_schema_claim_no_inbound_ide_door`.
- [x] Run strict OpenSpec and documentation/schema gates.
      Evidence: batch-81 gates are `just openspec-validate` and
      `python3 scripts/delivery_state.py validate`.
