# Chunk 1 — core sound references and pure routing

## Scope

Implement the substrate-free configuration and policy layer. This chunk is
self-contained after the existing notification route and must not invoke a
filesystem, environment, terminal, process, tokio, or termwiz API.

## Files touched (exact paths)

- `crates/thegn-core/src/lib.rs`
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_notifications.rs` (new; extract the notification
  config types currently in `config.rs`, including `NotificationsConfig`,
  `SoundMode`, `SoundConfig`, `NotificationRule`, `DndConfig`,
  `NotificationsOverlay`, and their pure helpers)
- `crates/thegn-core/src/notification_sound.rs` (new)
- `crates/thegn-core/src/notification_route.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-core/src/config_tests.rs`
- `crates/thegn-core/tests/config_example.rs`

Do not touch host files, docs, openspec files, ratchet files, DB migrations,
control schemas, or capability catalog files in this chunk.

## Approach

1. Extract notification config types using the existing `config_*` module
   pattern. Preserve public `thegn_core::config::*` paths through re-exports so
   unrelated callers do not need a broad migration. Avoid adding any new
   notification fields to the old god-file block.
2. Add `SoundRef` parsing and a bounded `volume` accessor in
   `notification_sound.rs`. Accept only `off`, `bell`/`terminal`,
   `builtin:bell`, `pack:<non-empty-name>`, and explicit absolute/tilde paths.
   Do not accept a command form from `per_kind`.
3. Add `mute`, `pack`, `volume`, and `per_kind` to `SoundConfig`; change the
   documented/default generic mode to terminal `bell`; preserve legacy
   `chime_file`, `command`, and `per_priority` deserialization for existing
   files. Add pure validation for volume, pack syntax, known kind names, and
   sound references. Unknown config kind keys must produce a useful error with
   a did-you-mean suggestion, not silently create a second catalog.
4. Change route output to carry the parsed sound reference and volume. Apply
   mute, route mute, DND, focused suppression, min-priority, and
   `always_kinds` before the resolution order in the design. Preserve legacy
   rule/command behavior without making per-kind strings shell commands.
5. Update `Config::effective_notifications` trust handling so untrusted repo
   overlays cannot supply pack paths, per-kind paths, legacy chime paths, or
   commands. Keep this pure data filtering; no filesystem existence check in
   core.
6. Add unit tests for parser aliases/rejection, policy precedence, all gates,
   default bell, volume bounds, unknown kinds, profile/overlay inheritance, and
   trusted-vs-repo sound fields. Update existing tests that assert the current
   synthesized-chime default.

## Overlap/dependency

No file overlap with chunks 2 or 3. Chunk 2 depends on the public config and
route types from this chunk; chunk 3 depends on the final key names and default
semantics. The Lead must run this chunk first, then chunk 2, then chunk 3.

## Tests to run

Run only scoped checks; do not start a full-workspace build:

```text
just quick thegn-core
cargo nextest run -p thegn-core notification_sound
cargo nextest run -p thegn-core config_notification
cargo nextest run -p thegn-core config_example
cargo nextest run -p thegn-core env_overlay
```

The env-overlay command is a ratchet verification: no new environment key is
expected because the new settings are nested structured config.

## Done criteria

- `thegn-core` has no substrate dependency or platform conditional for sound.
- `NotificationKind::ALL` remains the sole event catalog.
- Mapping and gating are deterministic pure functions with unit coverage;
  missing packs/files are not checked by core.
- Default behavior is terminal BEL, and `per_kind` custom audio is opt-in.
- Untrusted repo overlays cannot introduce sound paths or executable commands.
- Existing public config imports and unrelated notification behavior still
  compile under the scoped checks.
- The coder commits this chunk exactly as:

  `feat(the-35): add pure configurable sound policy`
