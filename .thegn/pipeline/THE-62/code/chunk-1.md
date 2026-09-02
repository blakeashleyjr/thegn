# Chunk 1 — core named push routing and pure rendering

## Scope

Extend the substrate-free notification model so one event can resolve to named
push sinks and render a provider-neutral/per-platform message. This chunk is
file-disjoint from chunks 2 and 3, but must run after THE-35 lands/rebases its
single `NotifyState`/sound route because `notification_route.rs` and the
notification config seam overlap with that branch. Chunk 2 consumes the core
types; chunk 3 consumes both core and svc APIs. Run chunks serially in that
order.

## Files touched (exact)

- `crates/thegn-core/src/config_push.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-core/src/notification_route.rs`
- `crates/thegn-core/src/notification_render.rs` (new)
- `crates/thegn-core/src/lib.rs`
- `config/config.toml.example`
- `docs/help/notifications.md`
- `openspec/changes/add-chat-webhook-sinks/proposal.md`
- `openspec/changes/add-chat-webhook-sinks/design.md`
- `openspec/changes/add-chat-webhook-sinks/tasks.md`
- `openspec/changes/add-chat-webhook-sinks/specs/notifications/spec.md`

Do not touch THE-35 sound behavior except resolving merge conflicts in the
shared route/config seam while preserving THE-35's `SoundEmit` and one-route
contract.

## Approach

1. Add `PushSinkConfig` and `PushConfig.sinks` as a nested array compatible
   with the existing `[notifications.push]` scalar fields and nested command
   inbox. Materialize the legacy scalar form as one sink named by its kind when
   `sinks` is empty. Add bounded name/array validation, duplicate detection,
   implemented/reserved kind validation, per-sink `min_priority`, and
   `env:`/`file:`-only URL validation for webhook/Discord/Slack. Do not add
   `keyring:` or literal URL support.
2. Change the pure `RouteDecision` push field to a deterministic named target
   collection. Preserve default `push` behavior as all effective sinks;
   recognize `push:<name>`; reject unknown selectors in strict validation;
   apply each sink floor after final priority/DND/rule evaluation; keep unknown
   notification kinds from pushing.
3. Add `notification_render.rs` with `MarkdownFlavor`, immutable rendered
   output, per-`NotificationKind::ALL` built-in templates, safe escaping, and
   character-based truncation. Keep timestamp an input, not a clock read.
   Providers must be able to request Discord, Slack, CommonMark, or plain
   rendering without core importing HTTP or a vendor SDK.
4. Update config example/help and the openspec draft to document exact nested
   TOML, secret custody, event templates/flavors, routing semantics, bounded
   delivery, Monitor counters, and doctor’s offline dry-run. Explicitly record
   the bot/gateway rejection and ntfy compatibility boundary.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core config_push`
- `cargo nextest run -p thegn-core notification_route`
- `cargo nextest run -p thegn-core notification_render`
- `cargo nextest run -p thegn-core config_example`
- `cargo nextest run -p thegn-core env_overlay_coverage`
- `cargo nextest run -p thegn-core crate_boundaries`

Tests must cover legacy single-sink parsing, named sink fan-out and targeting,
unknown sink errors, per-sink floors, raw URL rejection without echoing the
URL, all event templates, all markdown flavors, Unicode/Discord truncation,
stable generic fields, unknown-kind conservative routing, and no env-overlay
surface expansion.

## Ratchets

The config example and notifications help page are updated in this chunk.
There is no new env overlay, completion slot, capability catalog row, control
schema field, or help action; verify their existing ratchets rather than adding
an exemption. Keep `test/env-overlay-ratchet.txt`,
`test/completion-slot-ratchet.txt`, control snapshots, and async-trait ratchet
unchanged unless a test identifies an accidental surface. Any ignored result
introduced by pure validation must be annotated with its best-effort reason and
ratcheted in the normal file, not silenced globally.

## Done criteria

- Core has no HTTP/runtime/terminal/filesystem dependency and the focused
  renderer/route/config tests pass.
- Existing `[notifications.push]` ntfy + inbox configuration parses with the
  same behavior; named sinks parse only under `notifications.push.sinks`.
- A rule can target all sinks or one named sink, and an unconfigured target is
  a strict validation error naming the sink.
- Raw webhook/Discord/Slack URLs are rejected without appearing in any error.
- Config example/help/specs describe every key and the no-bot decision.
- Commit exactly as: `feat(the-62): extend core notification routing for named chat sinks`
