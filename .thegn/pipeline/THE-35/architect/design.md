# THE-35 — configurable notification sound effects

## Decision

Add configurable sound references to the existing notification route. The
`thegn-core` half remains pure: it parses sound references and resolves the
event-to-sound policy. The host half owns pack inspection, provider selection,
bounded off-loop playback, terminal-bell latching, and `thegn doctor` output.

The user-visible model is:

```text
notification producer
        │  one NotifyState route
        ▼
pure core decision: mute / gate / event → SoundRef
        │
        ├─ Bell: atomic latch + one loop waker
        └─ File: bounded queue → host provider → best-effort child
```

The default is the terminal bell. A custom file is never needed for a working
zero-config install. A custom effect is opt-in by adding a key under
`[notifications.sound.per_kind]`; an unmapped kind follows the existing
generic sound mode and gates. There are no bundled audio files and no code
synthesis of a default audio asset.

## Current branch findings and draft changes

The existing openspec draft is useful as a policy sketch, but several of its
claims do not describe this branch:

- `NotificationsConfig` and `SoundConfig` are still in the large
  `thegn-core/src/config.rs` block (`crates/thegn-core/src/config.rs:4494-4820`),
  while `thegn-core/src/config_notifications.rs` does not exist. New sound
  types must be extracted into a focused module rather than growing the
  config god-file. The existing module split pattern is visible in
  `crates/thegn-core/src/config_activity.rs` and the exports in
  `crates/thegn-core/src/lib.rs:36-64`.
- The implementation default is currently `SoundMode::Chime` and the current
  host materializes a synthesized WAV (`config.rs:4671-4683`,
  `config.rs:4771-4819`, `crates/thegn-host/src/chime.rs:47-65`). This conflicts
  with the base notification contract's terminal-bell zero-config behavior and
  with the requested no-bundled-assets preference. The target default is
  `bell`; `chime` remains a compatibility mode only when a user supplies a
  file.
- `EventBus` has a sound receiver (`crates/thegn-core/src/event_bus.rs:414-507`),
  but the startup subscriber is fed by an unconnected/partial path and typed
  producers still call `NotifyState::emit_sound` directly
  (`crates/thegn-host/src/run.rs:6729-6750`, `run.rs:8584-8597`,
  `run.rs:10129-10136`). It must not become a second sound funnel. The route
  helper owns one decision and one emission for each eligible notification.
- The live “agent needs you” signal is not an inbox event. The daemon writes
  `session_attention` and optionally a deduplicated inbox row
  (`crates/thegn-host/src/daemon/session.rs:843-915`); the host reads and folds
  those rows into the attention score (`crates/thegn-host/src/attention_status.rs:208-225`).
  Sound must observe an edge in that live state, with a baseline on startup,
  rather than inserting a second inbox row or sounding on every hydration.
- The current host chime module has platform conditionals, shell-quoted player
  commands, and player discovery outside `src/platform`
  (`crates/thegn-host/src/chime.rs:67-166`). It is replaced by a portable
  orchestration module plus `crates/thegn-host/src/platform/sound.rs`; this
  removes `chime.rs` from `test/platform-cfg-host-ratchet.txt`.
- The existing `SoundEmit::Command` is an explicit user-configured shell
  escape hatch (`crates/thegn-core/src/notification_route.rs:193-239`,
  `crates/thegn-host/src/notify.rs:338-360`). Preserve it as a trusted-config
  compatibility path, but do not allow `per_kind` values to become arbitrary
  shell commands. Provider playback uses fixed argv vectors.

The draft's synthesized sound family, mandatory filesystem validation in core,
new SQLite state, a new sound CLI action, and an additional event-bus sink are
cut. They either violate the substrate-free core/provider rules, add a second
source of truth, or add capability/control/help surface without a user action.

## Configuration contract

Keep the existing `[notifications.sound]` section and add only these fields:

```toml
[notifications.sound]
mute = false
mode = "bell"                 # bell | chime | command | off
min_priority = "alert"
always_kinds = ["agent_done", "agent_attention", "agent_failed"]
suppress_focused = true
pack = ""                     # trusted directory; empty means no pack
volume = 1.0                   # 0.0..=1.0, provider hint
chime_file = ""               # legacy generic chime file
command = ""                  # legacy command mode
per_priority = {}              # legacy command-mode overrides

[notifications.sound.per_kind]
# Keys are NotificationKind snake_case names. Values are SoundRef strings.
# agent_attention = "pack:attention"
# agent_done = "/home/me/sounds/finished.wav"
# queue_landed = "pack:merged"
# log_error = "pack:error"
```

`SoundRef` is the one core vocabulary used for per-kind policy:

- `off`/`none` disables that kind;
- `bell`/`terminal` selects the terminal bell;
- `pack:<name>` selects `<name>` from the configured trusted pack directory;
- an absolute or `~`-expanded user path selects a user-provided file;
- `builtin:bell` is an explicit alias for the bell and is the only built-in
  shipped by this issue.

Bare pack names are not accepted: the `pack:` prefix makes a path-vs-pack
decision obvious. Supported file formats are provider-dependent and reported
by `doctor`; a missing/unreadable reference falls back to the terminal bell.
The mapping is not a command language. `notifications.rules.sound`,
`command`, and `per_priority` retain their current command-mode compatibility
semantics and are resolved after the new per-kind reference according to the
policy below.

`pack` and file references are trusted-user configuration only. When a repo
overlay is loaded, `Config::effective_notifications` already strips command
execution from untrusted repo config (`crates/thegn-core/src/config.rs:6527-6561`);
the implementation must also clear `pack`, `per_kind`, `chime_file`, command,
and priority command overrides from that overlay. Global and selected-profile
configuration remains the place for sound paths.

`volume` is clamped/validated in core and is only a hint. A provider that lacks
volume control reports that capability and receives the default invocation;
the event loop never treats that limitation as an error.

## Pure core policy

Add a focused `thegn_core::notification_sound` module and use it from
`notification_route.rs`. The module owns `SoundRef`, parsing, validation, and
the policy result. It may refer to `NotificationKind`, `Priority`, and config
data, but not to paths, processes, terminals, tokio, termwiz, or environment
variables.

Use `NotificationKind::ALL` and `NotificationKind::as_str()` as the only event
catalog (`crates/thegn-core/src/notification.rs:157-221`). Validate
`per_kind` and `always_kinds` against that catalog with the existing
did-you-mean style. Unknown durable notification kinds keep the current
conservative behavior: they may be recorded/displayed but never sound or push.
Do not create a second sound-kind enum.

The issue's named events map to existing catalog kinds: “agent needs you” is
`agent_attention`; “agent finished” is `agent_done`; merge completion is
`queue_landed`/`pr_queue_merged` at the producer that knows which queue it is;
and “error” is the relevant existing `agent_failed`, `process_failed`,
`test_failed`, or surfaced `log_error` kind. Do not collapse those into a new
generic `error` kind.

The pure resolution order is:

1. global mute, route mute, DND/focused suppression, and priority gates;
2. matching rule sound override, if present;
3. `per_kind[kind]`, if present;
4. existing `per_priority[priority]` for command mode;
5. the generic `mode` (`bell`, legacy `chime_file`, `command`, or `off`).

`always_kinds` bypasses only `min_priority`, as today; it does not bypass mute,
DND, a rule route that excludes sound, or focused-worktree suppression. A
`per_kind` entry does not make an event audible by itself when the normal gates
reject it. A `pack:<name>` result is still an opaque reference at this layer.

Change `SoundEmit` from a host-shaped `Chime` variant to a pure file/reference
variant carrying the parsed `SoundRef` and volume. Keep `Bell` and `Command`
only as policy outputs. `RouteDecision` stays the compositor-facing value and
continues to make all channels independently observable.

Core tests must cover parser aliases and rejection, all precedence edges,
mute/off/DND/focused/min/always gates, unknown kinds, volume bounds, malformed
kind names, and the default bell. Add config overlay and trusted repo-overlay
tests. Do not test filesystem/player behavior in `thegn-core`.

## Host provider and playback

Add `crates/thegn-host/src/platform/sound.rs` as the only implementation file
that knows platform player names and conditionals. It exposes a small object-safe
provider seam, for example:

```rust
trait SoundPlayer: Send + Sync {
    fn id(&self) -> &'static str;
    fn caps(&self) -> SoundCaps;
    fn probe(&self) -> ProbeReport;
    fn play(&self, path: &Path, volume: f32) -> Result<(), SoundError>;
}
```

The exact trait location may be `thegn-host/src/platform/sound.rs`; do not add
an async method or an `async-trait` dependency. `SoundCaps` must state at least
file formats and volume support. The platform factory is the only place that
mentions `paplay`, `aplay`, `afplay`, PowerShell, or any other vendor/player.
Use fixed `Command::new(program).args(args)` and never `sh -c` for provider
paths. The existing configured command mode remains the separately documented
shell boundary and remains off the loop.

Add `crates/thegn-host/src/notification_sound.rs` for portable orchestration:

- scan/validate a configured pack once at startup and on config reload, off the
  loop; store an immutable snapshot of names to paths;
- resolve `SoundRef` from that snapshot without per-event filesystem access;
- create a bounded `SyncSender<SoundJob>` and a named `notify-sound` worker only
  when file/command playback is needed;
- set `platform::qos::Qos::Utility` at worker start;
- use `try_send`, drop with a diagnostic counter when full, and never wait in a
  producer or compositor path;
- on missing pack/file/provider/unsupported format/spawn failure, trace a
  best-effort diagnostic and ring the terminal bell where the policy requested
  an audio file; no error crosses into the compositor;
- coalesce only terminal-bell latches (the existing atomic latch already does
  this), not distinct queued file jobs;
- swap the playback snapshot atomically/under a short mutex during reload;
  reload itself must build the snapshot before taking the live lock.

`NotifyState::record`/its replacement must be the single host route that emits
sound after the core decision. Existing direct `emit_sound` calls in
`run.rs`, `pty_drain.rs`, `remote_poll.rs`, and queue handlers must be removed
to prevent duplicates. The route must still record first, then enqueue sound,
toast, and push according to the same `RouteDecision`. DB writes remain
best-effort cache writes and never become a reason to block the loop.

Audit typed producers while making this funnel: agent done/failed/attention,
test failure, process failure/exit, queue landed, worktree created, and log
error must pass through the known-kind route. Existing durable-only arbitrary
rows such as plugin/provider/disk bookkeeping and the CLI/daemon paths that do
not own a `NotifyState` remain durable-only; they must not be guessed into a
sound kind. The separate daemon's optional `agent_attention_inbox` row must
not be used as the playback trigger.

For live attention, add an edge observer next to the existing
`list_session_attention` fold in `attention_status.rs`. On the first snapshot,
seed `(session, since)` values without sound. Thereafter, a new session or a
changed `since` routes one synthetic `AgentAttention` cue through `NotifyState`;
removed/cleared sessions are forgotten. It must be called on the hydration
worker, never from render, and must not insert another notification row. This
preserves the live-state clearing semantics and prevents a cue on every 5-second
hydration.

## Doctor and capability boundaries

Add the sound provider to the existing `thegn doctor` provider-report surface,
but keep instantiation host-side: `thegn-svc` cannot construct a host platform
player. `thegn doctor` text and JSON must show provider id, availability,
supported formats, volume capability, selected pack path, pack entry count, and
the fallback reason. Missing optional players, packs, and files are diagnostics,
not doctor failures. Remove the standalone macOS `afplay` integration row in
`cmd/doctor.rs:2026-2045` so it cannot contradict the sound provider report.

This is not a control verb, MCP tool, completion slot, or wire-schema change.
Do not add a row to `thegn_core::capability::CATALOG`, a CLI action, a control
snapshot field, or a completion slot. `thegn_core::seam::{Probe,ProbeReport}`
(`crates/thegn-core/src/seam.rs:105-160`) is the appropriate probe vocabulary;
the implementation remains host-owned because it invokes platform players.

## Documentation and ratchets

Document every field, accepted value, trust boundary, fallback, and provider
capability in `config/config.toml.example` and `docs/help/notifications.md`.
Update the existing openspec draft's proposal/design/tasks/spec delta to match
this decision, especially the default bell, live attention edge, no synthesized
family, and no new DB/control surface.

Ratchet treatment is deliberate:

- remove `chime.rs` from `test/platform-cfg-host-ratchet.txt` after moving all
  player conditionals into `src/platform/sound.rs`;
- add no env overlay: all new fields are nested under `notifications.sound`,
  while `Config::env_overlay` intentionally exposes only shallow operational
  knobs (`crates/thegn-core/src/config.rs:5556-5625`); run the env-overlay
  coverage test and preserve its ratchet;
- add no completion slot, help action, capability catalog row, or control
  schema field; run their ratchet/snapshot tests to prove no accidental surface
  was added;
- keep best-effort ignores annotated for the ignored-result ratchet and keep
  the async-trait ratchet empty.

## Delivery order

The chunks are file-disjoint and should be applied serially because the later
chunks consume the types and APIs introduced by the earlier ones:

1. core sound references, config extraction, and pure route tests;
2. host provider, queue, live-attention observer, and producer migration;
3. example/help/spec updates, doctor presentation, and ratchet verification.

No `just test`, `just ci`, full-workspace compile, or e2e run is part of this
issue's implementation loop.
