# THE-21 chunk 2 — host runtime, catalog surfaces, CLI, and visibility

## Scope

Wire the core contract into the existing notification/session producers,
off-loop bounded executor, control surfaces, CLI, help, and Monitor/inbox
visibility. This chunk runs serially after chunk 1.

## Files touched

Host runtime and CLI:

- `crates/thegn-host/src/automation_events.rs` (new)
- `crates/thegn-host/src/automation_runtime.rs` (new)
- `crates/thegn-host/src/automation_executor.rs` (new)
- `crates/thegn-host/src/notify.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/hydrate_tracker.rs`
- `crates/thegn-host/src/hydrate_feed.rs`
- `crates/thegn-host/src/measure/disk.rs`
- `crates/thegn-host/src/daemon/session.rs`
- `crates/thegn-host/src/daemon/service.rs`
- `crates/thegn-host/src/handlers/calendar.rs`
- `crates/thegn-host/src/handlers/merge_queue.rs`
- `crates/thegn-host/src/handlers/pr_queue.rs`
- `crates/thegn-host/src/handlers/plugins.rs`
- `crates/thegn-host/src/handlers/provision.rs`
- `crates/thegn-host/src/handlers/repo_trust.rs`
- `crates/thegn-host/src/cmd/mod.rs`
- `crates/thegn-host/src/cmd/automations.rs` (new)
- `crates/thegn-host/src/cmd/notify.rs`
- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/cli_help.rs`

Control/catalog adapters and snapshots:

- `crates/thegn-svc/src/control/mod.rs`
- `crates/thegn-svc/src/control/routes.rs`
- `crates/thegn-svc/src/control/http.rs`
- `crates/thegn-svc/src/control/grpc.rs`
- `crates/thegn-svc/src/control/client.rs`
- `crates/thegn-core/src/control.rs`
- `crates/thegn-core/src/capability.rs`
- `crates/thegn-core/src/mcp/state.rs`
- `crates/thegn-svc/tests/control_schema.rs` only for snapshot generation
- `docs/api/control-v1.json`
- `docs/help/automations.md` (new)
- `test/env-overlay-ratchet.txt`
- `test/completion-slot-ratchet.txt`
- `test/surface-gaps-ratchet.txt` only if a catalog row is intentionally
  narrowed; prefer no new gaps
- `test/help-ratchet.txt` and/or `test/help-prose-ratchet.txt` only when the
  help ratchet identifies actual new documentation debt

Do not touch chunk 1 core model/config/DB files.

## Approach

1. Instantiate one automation runtime per notification-producing host/daemon
   process. Add one canonical notification emission helper that applies the
   existing pure route decision, preserves `put_notification_once` behavior,
   records the normalized event, and submits it once. Migrate every direct
   producer listed above, including daemon `notify_push`; do not bolt on a UI
   tap that misses direct DB writers. Keep `agent_attention` live-state
   semantics and use the daemon blocked/attention edge as its event.
2. Subscribe to `SessionActivityEvent`/`SessionExit` at the daemon service edge.
   Use `error_active` for THE-89 failure transitions. Derive worktree idle from
   event timestamps and a bounded worker deadline only while an idle rule is
   configured. Reuse the existing disk scan/result and PR/queue facts; never
   launch another poller or infer forge facts from message strings.
3. Execute plans in a bounded off-loop worker: bounded channel, semaphore,
   max queue/concurrency, per-action deadline, spawn_blocking for SQLite, and
   structured tracing fields (rule, event key, cap, run id, outcome). Record
   every outcome. Generated notifications and session origins are tagged and
   excluded by core loop prevention while remaining visible in Monitor/inbox.
4. Implement cataloged `tools.run` through the configured named-command and
   existing cap/sandbox path. Implement other actions through the existing
   `ControlApi`/catalog behavior: `sessions.open`, `merge.add`, and
   `notify.push`. Do not duplicate merge, agent, notification, or command logic.
5. Add `thegn automations list|test`; test is a pure dry run with a fixture and
   must not execute or write live state. Add JSON output and XDG-isolated tests.
6. Add control rows/routes/gRPC/MCP state tools for `automations.list` and
   `automations.test`; update API_CALLS, GRPC_CAPS, MCP_STATE_CAPS, scope tests,
   and the generated control schema through
   `THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`.
   Keep `cli_control_caps()` and plugin generic dispatch catalog-derived.
7. Register the action ids (`sessions.open`, `merge.add`, `notify.push`, and
   `tools.run`) in the one catalog, add grouped help/completion metadata and
   config/help prose, then update _all_ applicable ratchets in this same chunk:
   env-overlay, completion-slot, control-schema, surface-gaps, and help. Do not
   add a panel context key or fake surface gap.

## Dependency/overlap

Serial dependency on chunk 1’s `AutomationEvent`, config, store, notification
kinds, catalog ids, and v62 schema. No file overlap with chunk 1. The Lead may
parallelize neither chunk with chunk 1; this chunk is internally one cohesive
runtime/control change because splitting producers from the canonical emitter
would create a temporary duplicate event path.

## Tests to run

- `just quick thegn-svc`
- `just quick thegn-host`
- `just quick thegn-core`
- `cargo nextest run -p thegn-core env_overlay`
- `cargo nextest run -p thegn-svc control_schema`
- `cargo nextest run -p thegn-svc control`
- `cargo nextest run -p thegn-host automations`
- `cargo nextest run -p thegn-host notification`
- `cargo nextest run -p thegn-host daemon`

For CLI tests that open/read state, set `XDG_STATE_HOME` to a fresh temporary
directory. Use only focused crate filters; do not run `just test`, `just ci`, a
full-workspace compile, or e2e.

## Done criteria

- Every current notification producer reaches exactly one canonical event seam;
  direct writes preserve old routing/idempotence behavior.
- Session activity, exit, THE-89 failure, PR/queue, disk, and idle events are
  mapped without a render-loop ticker or duplicate scanner.
- Actions run only through catalog capabilities, with bounded off-loop
  execution, origin loop prevention, audit rows, logs, and visible outcomes.
- `thegn automations list` and pure `test` work, are documented, and are
  represented by catalog/control/snapshot/help/completion ratchets.
- Focused core, service, and host tests pass; no live state DB was migrated or
  used by a built binary.
- Commit exactly as: `feat(the-21): wire automation runtime and control surfaces`.
