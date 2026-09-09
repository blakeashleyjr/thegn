# Tasks — project vocabulary compatibility (THE-10)

- [x] Make project spellings canonical for config, environment, schema/help, and
      Home Manager while accepting workspace spellings for three stable releases.
- [x] Centralize compatibility normalization, canonical-wins diagnostics, and the
      named removal policy; preserve tracker-owned workspace/project keys.
- [x] Rename the existing multi-repo `project` CLI concept to `program`, retain
      warned compatibility aliases, and keep machine JSON stable.
- [x] Add canonical `program.*` catalog ids and deprecated `project.*` aliases
      with identical policy/projections.
- [x] Make project action ids canonical while accepting old workspace ids and
      retaining old vocabulary as palette/help search terms.
- [x] Update sidebar, palette, menus, prompts, help, README, examples, changelog,
      i18n keys, and generated-source inputs under the product/provider naming
      rules.
- [x] Preserve database tables, internal `Workspace*` types, cache/state keys,
      tracker fields, and Cargo/container uses.
- [x] Cover config/env aliases, duplicate precedence, CLI/action/catalog aliases,
      schemas, keymaps, help, and output compatibility with tests and review.
- [x] Land the accepted implementation series through `87042060` and strictly
      validate this change.

## Validation boundary

The accepted architecture explicitly did not claim a fresh broad e2e or full-CI
run in its design lane. Existing semantic/schema/compatibility gates are the
evidence recorded here.
