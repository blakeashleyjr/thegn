# Chunk 3 — configuration reference, openspec, and ratchet proof

## Scope

Document the final contract and reconcile the existing openspec draft. This
chunk is intentionally last so the prose, example, and scenarios match the
compiled names from chunks 1–2.

## Files touched (exact paths)

- `config/config.toml.example`
- `docs/help/notifications.md`
- `openspec/changes/add-notification-sound-packs/proposal.md`
- `openspec/changes/add-notification-sound-packs/design.md`
- `openspec/changes/add-notification-sound-packs/tasks.md`
- `openspec/changes/add-notification-sound-packs/specs/notifications/spec.md`

Ratchet files are intentionally not changed unless a scoped ratchet test proves
that a real new surface was introduced. In particular, do not add entries to
`test/env-overlay-ratchet.txt`, `test/completion-slot-ratchet.txt`,
`test/help-ratchet.txt`, `test/help-context-ratchet.txt`,
`test/help-panel-prose-ratchet.txt`, or a control schema snapshot for this
feature.

## Approach

1. Add a complete `[notifications.sound]` reference to the example: `mute`,
   `mode`, `min_priority`, `always_kinds`, `suppress_focused`, `pack`,
   `volume`, `chime_file`, `command`, `per_priority`, and the nested
   `per_kind` map. Explain every accepted value, path trust rule, provider
   fallback, and that custom effects are opt-in per kind.
2. Add a “Sound effects” section to the notifications help page covering every
   key, event names from the single catalog, global mute, gates, DND/focus,
   pack naming, provider detection, `doctor`, and best-effort behavior. Do not
   add a command/action merely to control sounds.
3. Rewrite the openspec draft where it is stale: default bell, no synthesized
   family, no bundled binary, fixed-argv provider seam under platform, bounded
   off-loop queue, pure core mapping, live attention edge, trusted overlay
   handling, and no DB/control/capability additions. Mark the portions already
   satisfied on this branch: the notification route, priority/DND/focus gates,
   command worker boundary, existing terminal-bell latch, and live attention
   state.
4. Run the help, env-overlay, completion, control-schema, platform-cfg, and
   ignored-result ratchets as scoped verification. Only shrink the host
   platform ratchet for the deleted `chime.rs`; do not pin new debt. If the
   help prose ratchet treats the notifications page as a required page, satisfy
   it by prose, not an allowlist line.

## Overlap/dependency

No file overlap with chunks 1 or 2. This chunk depends on their final API,
default, provider, and doctor terminology and must run after both. No code
changes belong here.

## Tests to run

```text
just quick thegn-core
just quick thegn-host
cargo nextest run -p thegn-core config_example
cargo nextest run -p thegn-core env_overlay
cargo nextest run -p thegn-host help
cargo nextest run -p thegn-host doctor
cargo nextest run -p thegn-svc control_schema
```

Also run the repository's scoped ratchet command if available for the platform
and ignored-result checks. Do not run e2e, `just test`, `just ci`, or a
full-workspace compile.

## Done criteria

- Every newly supported config key appears in both
  `config/config.toml.example` and `docs/help/notifications.md`.
- Openspec proposal/design/tasks/spec scenarios agree with the implementation
  and explicitly record the pruned draft claims.
- The env-overlay, completion-slot, help, control-schema, and ignored-result
  ratchets pass without adding unjustified surface; the platform ratchet only
  records the intended `chime.rs` removal.
- No documentation promises a provider, built-in audio asset, or config path
  that the implementation does not probe/resolve.
- The coder commits this chunk exactly as:

  `docs(the-35): document configurable sound effects and ratchets`
