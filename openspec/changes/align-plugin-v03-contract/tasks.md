# Tasks — align plugin v0.3 contract (THE-106)

- [ ] Inventory API version, wire types, extension points, host verbs, scopes,
      loader acceptance, runtime handlers, docs, examples, and snapshots.
- [ ] Add one canonical support-state table with version, capability/scope,
      mode, and owner for every extension point and host verb.
- [ ] Make loader negotiation and `plugin check` use or assert that table.
- [ ] Align module docs, developer guide, F1 help, examples, and generated schema
      to API v0.3 and exact runtime support states.
- [ ] Document/test `exec` as independent from `write` and `git`; `admin` implies
      all; contribution capabilities grant no host-call or exec scope.
- [ ] Mark PanelSection reserved/THE-108 and sidebar/theme/key-zone decisions
      reserved or absent/THE-107 without implying runtime support.
- [ ] Preserve v0.2 negotiation, decode, and byte-identical single-line view
      behavior; test unknown/newer values and stable rejection diagnostics.
- [ ] Add drift tests comparing support table, loader, schema, docs, examples,
      and `plugin check` output.
- [ ] Regenerate plugin schema and run plugin golden, scope, negotiation,
      documentation, strict OpenSpec, and full repository gates.
