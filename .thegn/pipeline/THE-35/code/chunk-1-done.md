# THE-35 chunk 1 completion

Implemented the substrate-free configurable sound policy:

- Extracted notification configuration into config_notifications.rs while
  preserving the public thegn_core::config::\* re-exports.
- Added pure SoundRef parsing for terminal bell, off, trusted pack names,
  and explicit absolute/tilde paths. Per-kind values cannot be commands.
- Added mute, pack, volume, and per_kind; the default generic mode is
  terminal bell and legacy chime/command fields remain compatible.
- Added pure sound-reference, volume, and catalog-kind validation with
  did-you-mean suggestions.
- Updated route resolution and SoundEmit to carry parsed references and
  volume, with gates applied before precedence resolution.
- Hardened untrusted repo notification overlays by clearing sound paths,
  packs, per-kind mappings, and command execution fields.
- Added parser, precedence/gate, validation, default, overlay, and trust-boundary
  tests.

Verification:

- just quick thegn-core — passed.
- cargo nextest run -p thegn-core notification_sound — 3 passed.
- cargo nextest run -p thegn-core notification_route — 36 passed.
- cargo nextest run -p thegn-core sound_config — 2 passed.
- cargo nextest run -p thegn-core repo_notification_overlay — 1 passed.
- cargo nextest run -p thegn-core env_overlay — 8 passed.

## Unverified

- cargo nextest run -p thegn-core config_notification matched no tests in
  this checkout and exited with nextest's “no tests to run” status.
- cargo nextest run -p thegn-core --test config_example passes its parse test
  but fails the documentation-completeness test for
  notifications.sound.mute, pack, and volume; documenting the example is
  owned by THE-35 chunk 3 and was intentionally not changed here.
- No full-workspace gate or e2e test was run, per the chunk and dev-loop policy.
