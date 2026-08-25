# Design — complete control-surface coverage

## Context: the audit ledger

One row per catalog domain, cells = surfaces declared × implemented today
(source: `CATALOG` + `SURFACE_GAPS` in `thegn-core/src/capability.rs`,
`ROUTES`/`API_CALLS` in `thegn-svc/src/control/routes.rs`, `GRPC_CAPS` in
`grpc.rs`, `cli_control_caps()` in `thegn-host/src/cmd/session.rs`,
`MCP_STATE_CAPS` in `thegn-core/src/mcp/state.rs`, `PLUGIN_HOST_CALL_CAPS` in
`plugin_api.rs`):

| Surface   | Implemented | Excused | Declared | Notes                                           |
| --------- | ----------- | ------- | -------- | ----------------------------------------------- |
| HTTP      | 31          | 1       | 32       | only `daemon.shutdown` missing                  |
| CLI       | 30          | 1       | 31       | only `daemon.shutdown` missing                  |
| gRPC      | 18          | 14      | 32       | 9 lag + 5 policy excuses                        |
| MCP       | 4           | 21      | 25       | retired by the in-flight MCP write-tools branch |
| Plugin    | 2           | 24      | 26       | generic dispatch + feed subscribe               |
| **Total** | **85**      | **61**  | **146**  | **58% covered**                                 |

Depth (cells the count cannot see): `browser.drive` answers
`Unimplemented("drive-browser")` on every surface (daemon
`service.rs:694`). `sessions.wait` was such a cell at audit time but all
`WaitCondition`s (`Exited`, `Idle`, `Blocked`, `Done`, `OutputMatches`) are
now implemented in `daemon/service.rs`.

## Decision 1: the ratchet is a committed allowlist, not prose

`SURFACE_GAPS` already fails a build when an excuse goes stale, but nothing
fails when an excuse is _added_ — "only shrinks" lives in a doc comment. We
adopt the house pattern (`test/*-ratchet.txt`): a committed
`test/surface-gaps-ratchet.txt` with one `capability<TAB>surface` line per
excused cell, loaded with `include_str!` by a `thegn-core` unit test
(compile-time, no I/O, works under the coverage gate) that asserts set
equality with `SURFACE_GAPS`. Growth therefore requires editing a file whose
header says additions need a written reason; shrink requires deleting the
line (loud in review, and `just ratchet-update`-style regeneration keeps it
honest). When the table reaches empty, the same test asserts
`SURFACE_GAPS.is_empty()` and the txt file is deleted.

_Alternative considered:_ a bare `assert!(SURFACE_GAPS.len() <= N)` count.
Rejected — a count lets one excuse be swapped for another invisibly; the
allowlist pins identities.

## Decision 2: permanent policy moves into the catalog, not the gap table

Five gRPC excuses are policy, not debt: pairing management and shutdown are
deliberately "HTTP + CLI only". Leaving policy in `SURFACE_GAPS` means the
table can never reach empty and every reader must sort debt from decree. The
fix: narrow those rows' `surfaces` declarations
(`SurfaceSet::of(&[Http, Cli])` for `pairings.*` and `daemon.shutdown`) and
delete the excuses. `SurfaceSet::OPERATOR` (http+grpc+cli) stays for any
future row that genuinely wants all three operator doors. The catalog remains
the single place a reader learns where a capability is reachable — which is
exactly where policy belongs.

## Decision 3: `daemon.shutdown` gets a route because the CLI needs one

The old excuse ("the daemon stops on signal / last-client policy, not by
request") predates supervising real fleets. The CLI client performs one-shot
HTTP over the control socket, so a `thegn daemon stop` verb _requires_ a
route; refusing the route means refusing the verb. `POST /v1/daemon/shutdown`
is admin-scoped: local unix-socket peers hold implicit admin
(`[serve] local_admin`), remote TCP callers need an admin token — the same
trust boundary pairing management already lives behind. Blast radius: a
leaked admin token could already revoke every pairing; being able to stop the
daemon adds denial-of-service, not data exposure, and admin tokens are
mintable only by an existing admin.

## Decision 4: plugin dispatch is generic, derived from the catalog

`PLUGIN_HOST_CALL_CAPS` is a hand-grown 2-entry table with a per-verb
dispatch arm — the exact drift shape the catalog exists to kill. The
archived `add-client-api-surface` change already built the generic spine:
`thegn api call` resolves `(method, path)` from `API_CALLS` and performs the
call over the control socket with JSON in/out. `host.call` reuses that spine:
any catalog row listing `Surface::Plugin` with a non-`WS` `API_CALLS` entry
dispatches generically after the existing `required_scope` check. Admin rows
are unreachable _by construction_ (no admin row lists `Surface::Plugin`; the
`admin_caps_never_reach_mcp_or_plugin` test pins it). `events.subscribe` is a
stream, not a request — it is bridged separately: a resident plugin that
declares an event subscription receives feed events as `on_event`
notifications, gated by `read` scope. `PLUGIN_HOST_CALL_CAPS` becomes derived
from the catalog (`for_surface(Plugin)` minus streams) instead of
hand-maintained.

## Decision 5: stubs are declared

A `stub: Option<&'static str>` field on `HostCapability` (naming what it
waits on) makes routed-but-501 visible: `thegn api list` and the coverage
report print it, and a unit test asserts a stub row's summary honesty the
same way `deprecated` rows work. `browser.drive` is the only stub today;
implementing it stays out of scope (there is no preview browser to drive
yet). This mirrors the seam convention of `kind` implemented-or-`reserved`.

## Decision 6: web-GUI lane judgment

**A served web GUI is not in thegn's lane now.** Reasons: (a) the product is
a terminal-native compositor with a working thin-client plan (pairing +
control API + event feed); a browser GUI would be a second front-end product
(terminal emulation in the browser, its own rendering stack) with no unique
value the TUI does not already deliver; (b) everything a GUI needs is API
work, and API work benefits every client. So this change ships **GUI
readiness**: complete surface coverage, one static `/pair` redeem page, an
opt-in CORS allowlist, and the rule that a browser client is an ordinary
paired thin client. A GUI, if it ever comes, is a plugin/companion artifact
consuming `/v1` with a `tgc1_` token — zero new policy surface.

**Serving stack (the THE-39 links):** pingora (Cloudflare) is a proxy
_framework_ for building network infrastructure — wrong altitude for an
embedded control API; vetis is a young HTTP server framework — adopting it
would add a second HTTP substrate beside the axum/hyper/tonic stack already
in `thegn-svc` for zero capability gain. Both rejected; axum stays. If thegn
ever fronts multi-instance fleets, a _separate_ pingora-based gateway could
proxy several daemons — noted for the future, not scoped.

**TLS:** v1 stays plaintext-behind-trusted-network (documented in `[serve]`);
the `fp` slot in the pairing URL already reserves certificate pinning as an
additive v2. Browser secure-context pressure (wss:// mixed content) is the
strongest argument to do TLS _before_ any hosted GUI — recorded as an open
question, not scoped here.

## Decision 7: audit records ride tracing

Mutating control calls (any verb whose `required_scope` is `Write`, `Git` or
`Admin`, plus every auth rejection) emit one structured event on target
`thegn::control::audit`: `pairing_id` (public id — safe to log by design),
`label`, capability id, target resource (session id / worktree path /
pairing id), and outcome (`ok` / `no_scope` / `unauthorized` / error class).
Token secrets never appear (only the id half is ever in scope at handler
level). Tracing is the house pipeline — free when `THEGN_LOG` is unset,
file-persisted when set — and a pure `thegn-core` record builder keeps the
field set unit-tested. _Alternative considered:_ a `control_audit` DB table
with a row cap; deferred — it needs retention policy and a viewer to be more
than a second log file, and the DB is a cache by doctrine. Revisit when a
real multi-user deployment exists. The plugin broker's in-memory
`AuditLogEntry` (plugin_api) stays as-is; the vocabulary (capability id +
action + outcome) is kept compatible so a future unified viewer can merge
them.

## Event loop / render / DB

All work is daemon/`thegn-svc`/CLI side: no render-path change, no new wake
source, no polling timeout (damage channel: none; the pairing overlay flow is
untouched). No SQLite schema change, no `user_version` bump. No new TUI
action/keybind/zone — no help-context claim needed; the config-reference help
page is generated, so `[serve] cors_origins` documents itself once the key
lands in `config.toml.example`.

## Security

- **New write surface:** `POST /v1/daemon/shutdown` — admin scope; see
  Decision 3 for blast radius. No other new mutating capability is added;
  everything else re-exposes existing verbs on more doors under the _same_
  `required_scope` table (never a second policy table).
- **Plugin dispatch blast radius:** bounded by declared plugin `scopes` ∩
  catalog surface sets; admin rows structurally unreachable; every dispatch
  is scope-checked before the control client is touched and audited on
  refusal.
- **`GET /pair` page:** unauthenticated static HTML like `/health`; the code
  rides in the URL fragment so it never reaches server logs; the page is
  fully self-contained (no external assets, CSP `default-src 'none'` with
  inline allowances), POSTs `/v1/pair`, and shows the minted token once
  without persisting it anywhere.
- **CORS:** default no cross-origin access (empty allowlist). `cors_origins`
  is an explicit origin list; wildcard is rejected at config validation —
  bearer-token APIs must never pair `*` with credentialed fetch.
- **Credential handling:** unchanged — 256-bit CSPRNG secrets, sha-256
  stored, constant-time compare, single-use codes, `require_approval`
  parking. No new credential kind, no raw tokens in config.
- **Unauthenticated endpoint exposure:** `/health`, `/v1/pair`, `/pair` are
  the only tokenless routes (the route test pins the list). Rate limiting on
  `/v1/pair` remains an open question (secrets are 256-bit so brute force is
  moot; the concern is junk-traffic DoS on a port the operator chose to
  expose).
- **Sandbox:** no implications — nothing here spawns user code.

## Open questions

1. TLS for `thegn serve` (activate the `fp` pin) — prerequisite for any
   hosted browser client beyond localhost; separate change.
2. Per-IP throttle on `/v1/pair` — worth it once `bind` ≠ loopback is common.
3. Should the coverage report also assert _depth_ (no `Unimplemented` returns
   from non-stub rows) via a conformance-style probe in `thegn doctor`?
4. Scope granularity: 4 scopes are coarse (a `git`-scoped phone can commit in
   _every_ worktree). Per-resource grants (worktree globs, like
   capability-grants) are a real future need for team deployments — explicitly
   out of scope here to keep one policy table.
