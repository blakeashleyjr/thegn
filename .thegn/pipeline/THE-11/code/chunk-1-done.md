# THE-11 chunk 1 completion

Implemented the core drawer metadata/catalog chunk.

## Delivered

- Added `DrawerScope` (`worktree`/`global`) through the shared `config_enum!`
  schema machinery and re-exported it from `config`.
- Added serde-defaulted `NamedCommand.drawer_scope` and `drawer_cwd` fields;
  existing tool/agent constructors initialize both to `None`.
- Added the pure `config_drawer` policy module. It keeps the built-in `files`
  occupant first, preserves eligible tool order, emits stable `tool:<name>` IDs,
  omits malformed/duplicate occupants with warnings, and provides scope-aware
  occupant filtering.
- Added no-I/O cwd validation/resolution and scope-key calculation. Worktree
  cwd values are relative and cannot escape via `..`; global values are absolute
  or `~`-prefixed.
- Added strict validation for drawer metadata, agent misuse, missing names or
  commands, duplicate IDs, and invalid cwd policy. Normal loading warns and
  degrades per occupant; agent-only metadata is stripped while keeping the
  agent usable.
- Updated the config enum count ratchet 90→91, env-overlay ratchet pins,
  config fixtures/tests, and `config/config.toml.example` with ATAC worktree and
  database global examples.

## Verification

- `just quick thegn-core` — passed.
- `cargo nextest run -p thegn-core drawer config_validate` — 30 passed.
- `cargo nextest run -p thegn-core env_overlay` — 8 passed.
- `git diff --check` — passed.

## Unverified

- Full-workspace gates, e2e, and chrome snapshot re-recording were intentionally
  not run per the chunk/dev-loop constraints.
- Host lifecycle/picker/chrome integration is owned by chunks 2–3 and was not
  verified here.
- No built binary, migration, or live state database was invoked.
