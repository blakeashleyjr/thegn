# Tasks — complete control-surface coverage

## 1. Ratchet + report (make the number visible and shrink-only)

- [ ] 1.1 `test/surface-gaps-ratchet.txt`: one `capability<TAB>surface` line
      per current `SURFACE_GAPS` entry, with the house header ("shrink-only;
      never add a line without a reason").
- [ ] 1.2 `thegn-core` unit test (`include_str!`, no I/O): set equality
      between the file and `SURFACE_GAPS`; when both are empty the test
      asserts `SURFACE_GAPS.is_empty()`.
- [ ] 1.3 `justfile`: extend the ratchet-update recipe to regenerate the file.
- [ ] 1.4 `thegn api coverage`: per-surface implemented / stub / excused /
      declared counts + the excused list (human table, `--json` via the one
      emitter). Pure ledger computation in `thegn-core::capability`
      (unit-tested); printing in `cmd/api.rs`.
- [ ] 1.5 `thegn doctor`: one summary line (cells implemented/declared, gap
      count).

## 2. Catalog honesty (thegn-core)

- [ ] 2.1 Narrow `pairings.issue/list/revoke/approve` and `daemon.shutdown`
      to `SurfaceSet::of(&[Http, Cli])`; delete their 5 gRPC excuses (and the
      ratchet lines).
- [ ] 2.2 Add `stub: Option<&'static str>` to `HostCapability`; mark
      `browser.drive`; unit tests (stub rows print in `api list` / coverage,
      a stub row cannot also be deprecated).
- [ ] 2.3 Update the catalog tests for the narrowed sets (the
      `required_for` / `coverage_problems` fixtures that name
      `pairings.issue` on Mcp etc. still hold).

## 3. gRPC parity (thegn-svc, feature `control-grpc`)

- [ ] 3.1 Proto: messages + RPCs for `sessions.wait`, `sessions.split`,
      `worktrees.list`, `merge.list/add/clear`,
      `calendar.events/clocks/ingest` (mirror the HTTP wire types).
- [ ] 3.2 Handlers adapting `ControlApi`, scope-checked via `required_scope`
      before dispatch, `ControlError` → gRPC status mapping as today.
- [ ] 3.3 Grow `GRPC_CAPS` to 27; delete the 9 excuses + ratchet lines;
      coverage test green.

## 4. `daemon.shutdown` (thegn-svc + thegn-host)

- [ ] 4.1 `POST /v1/daemon/shutdown` route (`caps: ["daemon.shutdown"]`,
      admin scope) → `ControlApi::shutdown`; `API_CALLS` row.
- [ ] 4.2 `thegn daemon stop` CLI verb over the control socket (graceful
      message when no daemon runs), following the
      `add-cli-namespaces-and-remote-open` noun-verb grammar.
- [ ] 4.3 Delete the HTTP + CLI excuses (+ ratchet lines); update the
      pinned open-route list test if touched.

## 5. Plugin surface (thegn-core + thegn-host)

- [ ] 5.1 Derive the plugin dispatch set from the catalog
      (`for_surface(Plugin)` minus streaming rows) and dispatch generically
      through the `API_CALLS` spine over the control socket; delete the
      hand-grown per-verb arms; scope-check unchanged
      (`required_scope`), refusals audited.
- [ ] 5.2 Delete the 23 `host.call` excuses (+ ratchet lines); the
      `plugin_host_calls_cover_catalog` test pins the derived set.
- [ ] 5.3 Resident-plugin feed subscribe: declared subscription → daemon feed
      events delivered as `on_event` notifications (read scope, off-loop,
      waker-pulsed like every plugin message); delete the `events.subscribe`
      plugin excuse. Coordinate frame vocabulary with
      `add-event-feed-subscriptions` if it lands first.
- [ ] 5.4 Unit tests: admin rows unreachable, scope refusal, unknown cap
      answers `unsupported`.

## 6. Web-GUI readiness (thegn-svc)

- [ ] 6.1 `GET /pair`: static self-contained redeem page (reads `#t=` from
      the fragment, POSTs `/v1/pair`, shows the minted token once); CSP
      header; ROUTES entry with `caps: &[]`; update the pinned
      unauthenticated-route list test (`/health`, `/pair`, `/v1/pair`).
- [ ] 6.2 `[serve] cors_origins = []`: explicit origin allowlist on the axum
      layer; wildcard rejected in `config_validate`; document the key in
      `config/config.toml.example`.
- [ ] 6.3 Smoke: `curl /pair` returns the page; a disallowed origin gets no
      CORS headers.

## 7. Audit records (thegn-core + thegn-svc)

- [ ] 7.1 Pure record builder in `thegn-core` (fields: pairing id, label,
      capability, target, outcome; never a secret) — unit-tested under the
      coverage gate.
- [ ] 7.2 Emit on target `thegn::control::audit` from the HTTP/gRPC adapters
      for every mutating verb and every auth rejection; test with a capturing
      subscriber.

## 8. Finish

- [ ] 8.1 After the in-flight MCP write-tools branch lands (retiring the 21
      MCP excuses), regenerate the ratchet file; if empty, flip the test to
      the empty-table assertion and delete the file.
- [ ] 8.2 Docs: `docs/ARCHITECTURE.md` capability-catalog section notes the
      ratchet; `docs/extending/` capability recipe mentions the file.
- [ ] 8.3 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
