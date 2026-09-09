# Tasks — external IDE inbound boundary (THE-104)

- [ ] Audit current docs, schemas, binaries, packaging, and editor actions for
      any accidental inbound claims.
- [ ] Write the no-inbound / paired-client / strict-URL decision matrix with
      security, portability, packaging, and maintenance costs.
- [ ] Decide whether inbound IDE integration is an alpha product surface.
- [ ] Publish the chosen support state and exact outbound-versus-inbound terms.
- [ ] Remove any stale `thegn://`, launcher, or extension claims if unsupported.
- [ ] If adopted, create a separate implementation OpenSpec change with catalog,
      auth, protocol, platform, tests, and documentation acceptance criteria.
- [ ] Add contract tests that public schemas/help cannot imply an unimplemented
      inbound door.
- [ ] Run strict OpenSpec and documentation/schema gates.
