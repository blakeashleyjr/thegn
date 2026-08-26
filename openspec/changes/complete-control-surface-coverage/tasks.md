# Tasks — complete control-surface coverage

## 1. Ratchet + report (make the number visible and shrink-only)

- [x] 1.1 `test/surface-gaps-ratchet.txt`: one `capability<TAB>surface` line
      per current `SURFACE_GAPS` entry, with the house header ("shrink-only;
      never add a line without a reason").
- [x] 1.2 `thegn-core` unit test (`include_str!`, no I/O): set equality
      between the file and `SURFACE_GAPS`; when both are empty the test
      asserts `SURFACE_GAPS.is_empty()`. (`ratchet_pins_surface_gaps`)
- [x] 1.3 `justfile`: extend the ratchet-update recipe to regenerate the file.
      (`surface_gaps_ratchet_update`, ignored, driven by `THEGN_RATCHET_UPDATE=1`)
- [x] 1.4 `thegn api coverage`: per-surface implemented / stub / excused /
      declared counts + the excused list (human table, `--json` via the one
      emitter). Pure ledger computation in `thegn-core::capability`
      (`ledger` / `SurfaceLedger`, unit-tested); printing in `cmd/api.rs`.
- [x] 1.5 `thegn doctor`: one summary line (cells implemented/declared, gap
      count).

## 2. Catalog honesty (thegn-core)

- [x] 2.1 Narrow `pairings.issue/list/revoke/approve` and `daemon.shutdown`
      to `SurfaceSet::of(&[Http, Cli])`; delete their 5 gRPC excuses (and the
      ratchet lines).
- [x] 2.2 Add `stub: Option<&'static str>` to `HostCapability`; mark
      `browser.drive`; unit tests (stub rows print in `api list` / coverage,
      a stub row cannot also be deprecated).
- [x] 2.3 Update the catalog tests for the narrowed sets (the
      `required_for` / `coverage_problems` fixtures that name
      `pairings.issue` on Mcp etc. still hold).

## 3. gRPC parity (thegn-svc, feature `control-grpc`)

- [x] 3.1 Proto: messages + RPCs for `sessions.wait`, `sessions.split`,
      `worktrees.list`, `merge.list/add/clear`,
      `calendar.events/clocks/ingest`. Simple types (Wait, Split, WorktreeInfo)
      mirror the HTTP wire; the rich serde payloads (MergeQueueRow, CalEvent)
      ride as JSON strings — the same payload the HTTP surface returns.
- [x] 3.2 Handlers adapting `ControlApi`, scope-checked via `required_scope`
      before dispatch, `ControlError` → gRPC status mapping as today.
- [x] 3.3 Grow `GRPC_CAPS` to 27; delete the 9 excuses + ratchet lines;
      coverage test green.

## 4. `daemon.shutdown` (thegn-svc + thegn-host)

- [x] 4.1 `POST /v1/daemon/shutdown` route (`caps: ["daemon.shutdown"]`,
      admin scope) → `ControlApi::shutdown`; `API_CALLS` row.
- [x] 4.2 `thegn daemon stop` CLI verb over the control socket (graceful
      message when no daemon runs). Added as an optional subcommand on the
      hidden `Daemon` command (bare `thegn daemon` still runs the daemon).
- [x] 4.3 Delete the HTTP + CLI excuses (+ ratchet lines); updated the pinned
      open-route list test (`/health`, `/pair`, `/v1/pair`) and the snapshot.

## 5. Plugin surface (thegn-core + thegn-host)

- [x] 5.1 Derive the plugin dispatch set from the catalog
      (`plugin_host_call_caps()` = `for_surface(Plugin)` minus streaming rows)
      and dispatch generically through the `API_CALLS` spine
      (`cmd::api::resolve_call` + `call_raw`) over the control socket; deleted
      the hand-grown per-verb arms; scope-check unchanged, refusals audited.
- [x] 5.2 Delete the 24 `host.call`/feed excuses (+ ratchet lines); the
      `plugin_host_calls_cover_catalog` test pins the derived set.
- [x] 5.3 Resident-plugin feed subscribe: a `host.call events.subscribe`
      (read scope) starts an off-loop bridge (dedicated thread + control-client
      `subscribe_events`) forwarding feed frames as `on_event` notifications;
      deleted the `events.subscribe` plugin excuse. Pane bytes never bridged.
- [x] 5.4 Unit tests: admin rows unreachable (`Unsupported`), scope refusal
      (`Denied`), unknown cap (`Invalid`).

## 6. Web-GUI readiness (thegn-svc)

- [x] 6.1 `GET /pair`: static self-contained redeem page (reads `#t=` from
      the fragment, POSTs `/v1/pair`, shows the minted token once); CSP
      header; ROUTES entry with `caps: &[]`; updated the pinned
      unauthenticated-route list test (`/health`, `/pair`, `/v1/pair`).
- [x] 6.2 `[serve] cors_origins = []`: explicit origin allowlist on the axum
      layer (tower-http `CorsLayer`, TCP listener only); wildcard rejected in
      `config_validate`; documented the key in `config/config.toml.example`.
- [~] 6.3 Unit tests cover the page (`pair_page_is_self_contained_and_unauthenticated`)
  and the wildcard rejection (`serve_cors_wildcard_...`); the disallowed-origin
  CORS behavior rides tower-http's tested `CorsLayer`. A `test/smoke.sh`
  curl step was NOT added (would need a live `thegn serve`).

## 7. Audit records (thegn-core + thegn-svc)

- [x] 7.1 Pure record builder in `thegn-core` (`control_audit::AuditRecord`;
      fields: pairing id, label, capability, target, outcome; never a secret) —
      unit-tested under the coverage gate.
- [x] 7.2 Emit on target `thegn::control::audit` from the HTTP (per-resource
      target) and gRPC adapters for every mutating verb and every auth
      rejection; tested with a capturing subscriber
      (`mutating_calls_and_rejections_emit_audit_records`).

## 8. Finish

- [~] 8.1 The MCP write-tools branch (already landed on main) retired only 4 of
  the 21 MCP excuses; **17 MCP excuses remain** and are the sole content of
  `test/surface-gaps-ratchet.txt`. The empty-table flip + file deletion is
  deferred to when the remaining MCP state tools land. Everything else (gRPC,
  HTTP, CLI, plugin, pairing/shutdown policy) is burned.
- [x] 8.2 Docs: `docs/ARCHITECTURE.md` capability-catalog section notes the
      ratchet, the coverage report, stubs, generic plugin dispatch and audit;
      `docs/extending/capability.md` mentions the ratchet file + derived plugin
      dispatch + the stub marker.
- [~] 8.3 Full `just ci` deferred (box saturation); instead ran scoped full
  `cargo nextest run -p <crate> --no-fail-fast`: thegn-core (2752 pass, after
  the `serve.cors_origins` env-overlay ratchet line) and thegn-svc
  control-grpc (524 pass) green; thegn-host in progress.
