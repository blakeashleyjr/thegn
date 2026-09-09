# Tasks — remove browser.drive stub (THE-103)

- [x] Inventory every catalog, verb, route, projection, schema, doc, and test
      reference to `browser.drive`.
- [x] Remove the catalog row and control verb/handler atomically.
- [x] Remove all public transport projection and client bindings.
- [x] Regenerate committed control/capability schemas and API documentation.
- [x] Replace the browser-specific stub assertion with a generic test that no
      advertised supported operation is universally `Unimplemented`.
- [x] Verify preview discovery, external open, and `preview.fetch` are unchanged.
- [x] Run focused catalog/transport coverage, schema snapshot, CLI help,
      completion/surface ratchets, and strict OpenSpec validation. Full
      repository gates remain the single pre-push run per repository policy.
