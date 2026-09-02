# Chunk 3 — host wiring, doctor presentation, and Monitor counters

## Scope

Wire named core decisions to the svc providers through the existing host
notification chokepoint, and make delivery loss observable in Monitor. This
chunk is file-disjoint from chunks 1 and 2, but serially depends on both; the
THE-35 branch must already own sound/live-attention routing. It must not add a
second route, duplicate attention cues, or introduce a timer.

## Files touched (exact)

- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/notify.rs`
- `crates/thegn-host/src/push_notify.rs`
- `crates/thegn-host/src/notification_delivery.rs` (new)
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/chrome.rs`
- `crates/thegn-host/src/monitor.rs`
- `crates/thegn-host/src/monitor/build.rs`
- `crates/thegn-host/src/monitor/notifications.rs` (new)
- `crates/thegn-host/src/monitor_tests.rs`
- `crates/thegn-host/src/cmd/doctor.rs`
- `test/platform-cfg-host-ratchet.txt`
- `test/ignored-result-ratchet.txt`

The platform ratchet is included for verification of no new misplaced
platform cfg; add a line only if an existing ratchet-driven move is required.
Do not touch `crates/thegn-host/src/attention_status.rs`: THE-35's hydration
edge observer is the sole live `agent_attention` trigger.

## Approach

1. Replace the single `push_tx`/provider assumption with one bounded worker
   job type carrying sink name plus rendered notification, and a provider map
   built from the effective config. `NotifyState::record` remains the only
   dispatch funnel: record first, then toast/sound/push from one decision. Fan
   out only to `RouteDecision.push_sinks`; a dropped provider never changes the
   durable inbox result. Remove any direct producer `emit_push` calls that would
   duplicate the route, while leaving THE-35's sound ownership intact.
2. Put queue/worker counters in a focused `notification_delivery.rs` snapshot
   (`Arc` + atomics or a lock-free immutable snapshot). Track per sink queue
   overflow, rate-limit drop, retry, sent, and terminal/dead-letter outcomes.
   Worker and queue failures identify only the sink name/status/class. A
   provider-final failure after bounded attempts increments dead-letter and
   does not retry forever. Config reload constructs new workers/providers before
   swapping the sender; stale workers drain/close without blocking the loop.
3. Make the worker use the existing off-loop QoS/channel/waker conventions.
   `try_send` is non-blocking; the worker owns its current-thread async runtime
   if needed and uses svc's provider future. A changed snapshot may pulse the
   existing waker only for an open Monitor; it does not create a periodic timer
   or perform HTTP in `run.rs`/render code.
4. Carry the snapshot into `FrameModel` and add a conditional Notifications
   tab/section to the established Monitor builder. Keep it loop-owned and
   non-persistent. Render sink rows with the existing semantic tokens and
   capability/degradation wording; in particular, show `dead-letter N` and
   distinguish queue/rate drops. Add focused pure monitor row tests.
5. Update `cmd/doctor.rs` only to present the svc registry's per-sink offline
   dry-run reports and redacted SecretRef state. Do not print `server`, `url`,
   or resolved tokens for chat sinks; preserve existing ntfy/inbox output and
   its compatibility semantics. Ensure `doctor --json` has caps/notes but no
   endpoint value.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host notify`
- `cargo nextest run -p thegn-host monitor`
- `cargo nextest run -p thegn-host doctor`
- `cargo nextest run -p thegn-host platform_cfg`

Tests must cover one route fan-out to named sinks, queue overflow, rate-limit
drop, retry then dead-letter, config reload swap, no worker when unconfigured,
no duplicate producer delivery, URL absence from text/JSON/trace diagnostics,
Monitor visibility/row formatting, and an existing ntfy-only route remaining
byte/behavior compatible. The smoke case should use a refusing localhost
endpoint with a temporary `XDG_STATE_HOME`; never invoke the binary against the
live state DB and do not run e2e.

## Ratchets

Run and keep green the host platform-cfg ratchet, ignored-result ratchet,
help/config ratchets, completion-slot ratchet, control-schema snapshot, and
capability catalog tests. No new action, CLI subcommand, completion slot,
control field, or env overlay is authorized. If the Monitor tab requires a
stable internal label, it is not a keymap action and must not be added to the
capability catalog. Any ignored result in the worker is annotated with the
existing best-effort convention. Update `test/platform-cfg-host-ratchet.txt`
only when a platform conditional is actually moved into its required module.

## Done criteria

- All host-routed notification producers reach chat through the one
  `NotifyState` route; THE-35 sound and live-attention semantics remain
  single-sourced. The durable-only `thegn notify push` CLI path is unchanged.
- Delivery is off-loop, bounded, best-effort, QoS-labelled, and never blocks or
  wakes continuously at idle. Queue/rate drops and post-retry dead letters are
  counted per sink and visible in Monitor.
- Doctor’s dry-run validates config/secret resolution and request shape without
  any network POST; URL/token values are absent from all human/JSON output and
  errors.
- Existing ntfy-only configs and notification inbox behavior remain intact;
  no migration, control surface, capability row, completion slot, or e2e test
  was added.
- Commit exactly as: `feat(the-62): wire chat sinks into host monitoring and doctor`
