# Design — configurable notification sound effects

## Existing route and pure policy

`thegn_core::notification_route::decide` is the single pure route decision.
It returns independently observable inbox, desktop, toast, push, and sound
channels. `NotifyState::record` in the host records first and then applies
that decision, so typed producers do not emit duplicate sounds directly.

The route applies global/route mute, DND, focused-worktree suppression, and
the sound priority floor before resolving the audible result. `always_kinds`
bypasses only `min_priority`; it does not bypass mute, a rule that excludes
sound, DND, or focused-worktree suppression. After those gates, precedence is:

1. a matched rule's `sound` action;
2. `[notifications.sound.per_kind][<kind>]`;
3. legacy `[notifications.sound.per_priority]` in command mode; and
4. the generic `mode` (`bell`, legacy `chime_file`, `command`, or `off`).

`SoundRef` is substrate-free. `off`/`none`, bell aliases, `builtin:bell`,
`pack:<name>`, and absolute/tilde paths are the complete per-kind vocabulary.
Core does not expand paths, inspect packs, read the environment, start a
process, or know a terminal player. Unknown catalog kinds and malformed
references are reported by config validation; runtime policy remains
conservative and falls back to the bell for an invalid file reference.

The existing rule `sound` field remains a trusted compatibility escape hatch:
`off` and bell aliases are policy values and other strings remain command-mode
commands. This compatibility behavior is intentionally separate from
`per_kind`, whose values can never become shell commands.

## Host snapshot and provider seam

`thegn-host/src/notification_sound.rs` owns orchestration. On startup and
configuration reload it builds the pack/file snapshot off the compositor loop,
then swaps it under a short lock. A `pack:<name>` lookup is a map lookup (the
snapshot indexes a pack filename and its stem); explicit file references are
expanded and checked while building the snapshot. No notification performs
filesystem access.

`thegn-host/src/platform/sound.rs` is the only file that knows platform player
names and conditionals. Its synchronous object-safe `SoundPlayer` seam reports
an id, supported formats, volume support, and a `ProbeReport`, then plays with
`Command::new(program).args(fixed_args)`. Provider paths never use `sh -c`.
The legacy configured command is the separate trusted shell boundary and is
run only by the off-loop worker.

File and command playback uses a bounded `SyncSender<SoundJob>`. Producers use
`try_send` and drop a file job with a diagnostic counter when the queue is
full. The `notify-sound` worker declares `Qos::Utility`; no producer or
compositor path waits for playback. Terminal bells use the existing atomic
latch and one loop-waker pulse, so simultaneous fallback requests coalesce.

Missing pack/file, missing provider, unsupported format, worker/spawn failure,
or provider failure is best-effort: the worker records a diagnostic and asks
the terminal-bell latch to fire. No playback error reaches the compositor.
There are no synthesized tones and no binary audio assets in the repository.

## Live attention edge

The daemon's `session_attention` rows remain live state, not a second inbox
trigger. The hydration worker folds the rows into the existing attention
status and keeps `(session, since)` values for sound observation. The first
snapshot seeds the baseline without sound. Later, a new session or changed
`since` routes one synthetic `AgentAttention` cue through `NotifyState`; a
cleared/removed session is forgotten. Render never performs this observation,
and no duplicate durable row is inserted.

## Trust and configuration

Global and selected-profile configuration may name trusted packs and files.
When an untrusted repository overlay is merged, sound `pack`, `per_kind`,
`chime_file`, `command`, and `per_priority` are cleared; a command mode is
also reduced to bell. This prevents a repository from causing host-side file
access or command execution merely by being opened.

`volume` is validated as a finite `0.0..=1.0` hint and clamped at use. A
provider without volume support receives its normal invocation and doctor
reports that volume is unsupported. Provider availability and pack state are
diagnostics, not failures of `thegn doctor`.

## Boundaries and pruned alternatives

- No second event-bus sound subscriber: it would duplicate the `NotifyState`
  route.
- No new SQLite state: the notification database is a cache, and live
  attention already has a source of truth.
- No new CLI action, control snapshot field, MCP tool, completion slot, or
  capability-catalog row: configuration and doctor reporting are sufficient.
- No in-process audio library: fixed-argv platform players preserve the
  existing process boundary and portability.
- No filename-convention `default.<ext>` or kind-derived fallback: a pack
  reference is explicit as `pack:<name>`, and an unresolved file always
  falls back to the terminal bell.
- No synthesized family or shipped default audio asset: the bell is the only
  built-in sound.

The notification route, priority/DND/focus gates, trusted command worker
boundary, terminal-bell latch, and live attention state are the branch's
existing/landed foundations; THE-35 composes configurable references on them.
