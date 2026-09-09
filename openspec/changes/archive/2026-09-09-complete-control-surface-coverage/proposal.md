# Complete control-surface coverage — the API audit made a ratchet

Linear: THE-39

## Why

The capability catalog (`thegn_core::capability::CATALOG`) is the one list
every external door projects, and per-surface coverage tests already stop
silent drift. But "coverage" today is 58%, and the excuse table is prose-only
shrink-only. The audit numbers, as of this proposal:

- **32 catalog rows** projected across 5 surfaces = **146 (capability,
  surface) cells**; **85 implemented (58%)**, **61 excused** in
  `SURFACE_GAPS`.
- Per surface: HTTP **31/32**, CLI **30/31**, gRPC **18/32**, MCP **4/25**,
  plugin **2/26**.
- The 61 excuses: gRPC 14 (9 "not yet mirrored" + 4 pairing-policy + 1
  shutdown-policy), MCP 21 (state tools), plugin 24 (generic dispatch + feed
  subscribe), CLI 1 and HTTP 1 (`daemon.shutdown`).
- **Depth gap** the cell-count cannot see: `browser.drive` is routed on every
  surface but answers `501 Unimplemented` unconditionally — it counts as
  covered while doing nothing.
- Nothing mechanical stops `SURFACE_GAPS` from _growing_: "only shrinks" is a
  doc comment, not a gate. Every other invariant in this repo that mattered
  ended up as a ratchet file; this one hasn't yet.

Separately, THE-39 asks for a web-GUI story. The honest audit finding:
`PairingUrl::web_form()` already advertises `http://host:port/pair#t=…`, but
no `GET /pair` route exists — the URL 404s. And `thegn serve` is plaintext
with no CORS story, so a browser-hosted client has no sanctioned path at all.

The goal is **100% coverage as a ratcheted, testable requirement**: every
declared cell implemented, every remaining excuse burned or converted into an
honest surface declaration, the gap table empty — and the control API complete
enough that a web GUI could be built later as a plain paired thin client with
**zero new policy surface**.

## What Changes

- **The gap table becomes a ratchet.** A committed shrink-only allowlist
  (`test/surface-gaps-ratchet.txt`, one line per excused cell) is pinned
  against `SURFACE_GAPS` by a `thegn-core` unit test: removing an excuse
  requires deleting its line; adding one fails until the file grows a line
  with a written reason — the same discipline as the architecture ratchets.
  The terminal state is an empty table, asserted by the same test.
- **Coverage becomes reportable.** `thegn api coverage` prints the per-surface
  ledger (implemented / stub / excused / total, and the gap list); `thegn
doctor` prints the one-line summary. Local introspection like `thegn api
list` — not a new catalog row.
- **gRPC reaches parity.** The 9 "not yet mirrored" verbs (`sessions.wait`,
  `sessions.split`, `worktrees.list`, `merge.list/add/clear`,
  `calendar.events/clocks/ingest`) get proto messages + handlers and their
  excuses burn. The 5 policy excuses (`pairings.*`, `daemon.shutdown` on gRPC)
  are converted into narrowed catalog surface declarations — the gap table is
  for _temporary debt_, not permanent policy.
- **`daemon.shutdown` becomes real.** `POST /v1/daemon/shutdown` (admin scope)
  plus `thegn daemon stop` on top of it; its HTTP and CLI excuses burn.
- **Plugin generic dispatch.** `host.call` dispatches _any_ catalog row that
  lists `Surface::Plugin` through the same route spine `thegn api call` uses,
  scope-checked by `required_scope(verb)`; 23 plugin excuses burn. A resident
  plugin can additionally subscribe to the control event feed via `on_event`,
  burning the last plugin excuse.
- **Stubs are declared, not silently 501.** Catalog rows gain a `stub` marker
  (`browser.drive` today); the coverage report counts stubs separately so
  routed-but-inert never reads as done.
- **Web-GUI readiness, not a web GUI.** `GET /pair` serves a tiny static,
  self-contained pairing-redeem page (fixing the advertised-but-404 URL);
  `[serve] cors_origins` (default empty) lets an operator opt a browser-hosted
  client in; a future GUI authenticates as an ordinary paired thin client —
  no cookies, no second auth table. The GUI itself is out of scope (see
  design.md for the pingora/vetis evaluation and the lane judgment).
- **Audit records.** Every mutating control call (write/git/admin scope) emits
  a structured audit record (pairing id, capability, target, outcome) through
  the host tracing pipeline — free when no subscriber is installed.

## Impact

- **Roadmap:** group **A 6** (one core, many front doors — this is its
  completion programme). The `wait` depth gap flagged by R 746 has since
  landed (all `WaitCondition`s are implemented in the daemon); recorded here
  for the audit trail.
- **Specs:** `capability-catalog` (MODIFIED gap requirement → ratcheted;
  ADDED coverage report; ADDED stub declaration), `control-plane` (ADDED gRPC
  parity, shutdown verb, pairing web page, browser-client policy, audit
  records), `plugin-runtime` (MODIFIED host.call dispatch; ADDED feed
  subscribe).
- **Code:** `thegn-core/src/capability.rs` (stub field, surface narrowing,
  ratchet test), `thegn-svc/src/control/{routes,http,grpc}.rs` + the proto,
  `thegn-host/src/cmd/{api,daemon}.rs`, plugin runtime dispatch,
  `config/config.toml.example` (`[serve] cors_origins`),
  `test/surface-gaps-ratchet.txt` (new).
- **In-flight dependencies:** the MCP write-tools branch (parameterised state
  tools, `--scopes`, retires the **21 MCP excuses**) is scoped elsewhere and
  is a hard dependency of the final empty-table assertion — this change does
  not re-scope it. `add-cli-namespaces-and-remote-open` owns the CLI grammar
  the new `thegn daemon stop` verb must follow. `add-fleet-view` consumes
  these surfaces but is not built on (its agent layer is excised).
  `add-event-feed-subscriptions` (THE-34, sibling change) evolves the feed the
  plugin subscribe bridge consumes.
- **No DB schema change**, no render-path change; everything runs daemon/svc
  side, off the render loop.
