# Tasks — remove browser.drive stub (THE-103)

- [ ] Inventory every catalog, verb, route, projection, schema, doc, and test
      reference to `browser.drive`.
- [ ] Remove the catalog row and control verb/handler atomically.
- [ ] Remove all public transport projection and client bindings.
- [ ] Regenerate committed control/capability schemas and API documentation.
- [ ] Replace the browser-specific stub assertion with a generic test that no
      advertised supported operation is universally `Unimplemented`.
- [ ] Verify preview discovery, external open, and `preview.fetch` are unchanged.
- [ ] Run catalog coverage, transport coverage, schema snapshot, CLI help,
      strict OpenSpec, and full repository gates.
