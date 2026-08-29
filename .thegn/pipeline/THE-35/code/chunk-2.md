# Chunk 2 — host provider, off-loop playback, and producer funnel

## Scope

Consume chunk 1's pure `SoundRef`/route API and implement the host-side provider
seam, bounded playback, doctor probe data, live attention edge detection, and
single notification emission path.

## Files touched (exact paths)

- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/chime.rs` (delete; all behavior moves to the two sound
  modules below)
- `crates/thegn-host/src/notification_sound.rs` (new)
- `crates/thegn-host/src/platform/sound.rs` (new)
- `crates/thegn-host/src/platform/mod.rs`
- `crates/thegn-host/src/notify.rs`
- `crates/thegn-host/src/attention_status.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/hydrate_feed.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/pty_drain.rs`
- `crates/thegn-host/src/remote_poll.rs`
- `crates/thegn-host/src/handlers/merge_queue.rs`
- `crates/thegn-host/src/handlers/pr_queue.rs`
- `crates/thegn-host/src/handlers/provision.rs`
- `crates/thegn-host/src/handlers/usage_alert.rs`
- `crates/thegn-host/src/handlers/calendar.rs`
- `crates/thegn-host/src/hydrate_tracker.rs`
- `crates/thegn-host/src/measure/disk.rs`
- `crates/thegn-host/src/cmd/doctor.rs`
- `test/platform-cfg-host-ratchet.txt`

Do not touch core files, user documentation, openspec files, completion
ratchets, env-overlay ratchets, control snapshots, or capability catalog files
in this chunk.

## Approach

1. Add a portable orchestration runtime in `notification_sound.rs`. Build pack
   snapshots and provider detection on startup/reload off the compositor loop;
   resolve queued jobs from the snapshot, not from per-event filesystem calls.
   Use a bounded `SyncSender`, `try_send`, a named Utility-QoS worker, and
   best-effort diagnostics. A full/closed queue, absent provider, bad file, or
   child failure must never be returned as a compositor error.
2. Put every `#[cfg]`, player name, and player-specific argv in
   `platform/sound.rs`. Implement an object-safe synchronous `SoundPlayer` with
   `caps` and `probe` data. Use fixed argv for `paplay`, `aplay`, `afplay`, and
   PowerShell; never shell-quote a provider path or pass it through `sh -c`.
   Keep the configured legacy command mode's explicit shell execution on the
   worker, with its existing warning/best-effort contract. Do not reintroduce
   synthesized WAV output.
3. Replace `chime` module wiring and remove the current inline `chime::play`
   calls. Make `NotifyState::emit_sound` enqueue a job or latch BEL only.
   Refactor the record/route helper so each eligible producer makes one core
   decision and emits sound exactly once. Remove the startup event-bus sound
   subscriber/typed duplicate path rather than adding a second sink.
4. Migrate the listed known-kind producers (agent done/failed/attention,
   process/test failures, queue landed, worktree created, log error, calendar,
   tracker, and disk events) to the route helper where they currently bypass
   it. Preserve durable-only behavior for arbitrary `plugin`, provider, and
   bookkeeping kinds and for separate daemon/CLI DB writers that have no
   `NotifyState`; do not map unknown strings to a soundable kind. Preserve
   log-error growth/stale-clear behavior while routing the newly-created
   `log_error` event when its existing surface flag allows it.
5. Add an edge observer adjacent to the existing attention-state fold. On the
   first live snapshot seed session identities/timestamps; later new or changed
   `since` values call the route helper once as `AgentAttention`; removed rows
   are forgotten. Never create an inbox row and never emit once per hydration.
   Keep this observer on the hydration worker.
6. Extend the existing host-owned doctor text/JSON provider report with sound
   provider, caps, selected pack, entry count, and fallback reason. Reuse the
   core `ProbeReport` vocabulary but do not put a host player factory in
   `thegn-svc` (that crate cannot instantiate platform code). Remove the
   duplicate macOS `afplay` integration row. Missing optional audio is
   informational.
7. Shrink the host platform ratchet by removing `chime.rs`; prove all new
   platform conditionals are under `src/platform/sound.rs`. Keep thread QoS and
   ignored-result annotations consistent with existing ratchets.

## Overlap/dependency

No file overlap with chunks 1 or 3. This chunk depends on chunk 1's public
types and route output and must run after chunk 1. Chunk 3 consumes the final
runtime/doctor names and must run after this chunk. Within this chunk, provider
and `NotifyState` changes are serial with producer migration; the Lead should
not parallelize those edits even though the final file list is disjoint by
sub-area.

## Tests to run

```text
just quick thegn-host
cargo nextest run -p thegn-host notification_sound
cargo nextest run -p thegn-host notify
cargo nextest run -p thegn-host attention_status
cargo nextest run -p thegn-host doctor
cargo nextest run -p thegn-host platform_cfg
```

Do not run e2e, `just test`, `just ci`, or a full-workspace compile. Tests must
cover fixed provider argv, caps/probe degradation, pack snapshot lookup,
bounded queue/drop behavior, BEL fallback, route de-duplication, and attention
baseline/edge/clear behavior.

## Done criteria

- No player implementation or platform conditional remains in `chime.rs` or
  another non-platform host module; `chime.rs` is deleted.
- No notification producer blocks on playback, waits on a child, scans a pack,
  or propagates playback failure into the event loop.
- Each known soundable event has exactly one route/emission; live attention is
  edge-triggered and does not duplicate the optional inbox row.
- The provider seam is object-safe, synchronous, reports caps, and is visible
  through the existing doctor provider surface.
- No new capability catalog row, control verb, SQLite table, or CLI action is
  introduced.
- The coder commits this chunk exactly as:

  `feat(the-35): add best-effort sound provider and routing`
