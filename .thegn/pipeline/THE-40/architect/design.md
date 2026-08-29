# THE-40: Native GUI frontend lane

Linear: THE-40; decision type: architecture / documentation only
Status: not now; preserve a future thin-client lane

## Decision

Do not build a GUI in THE-40. “Native GUI” is additive to thegn: a separate
frontend may consume the daemon and the control API, but it must not become a
second renderer inside `thegn-host`, a second capability registry, or a runtime
dependency of the shell.

If the work is reopened, the preferred shape is a separate GPU terminal-cell
client (candidate 2 below), with native chrome deferred until the substrate can
publish a stable chrome view model. A web client remains a valid consumer of
the same substrate, but is a remote/web-access lane rather than the native GUI
decision.

This is a decision record, not an implementation. It adds no Rust, frontend,
configuration, capability, database, wire, or ratchet changes.

## Evidence and corrections to the openspec draft

The draft in `openspec/changes/define-gui-frontend-lane/` was useful framing,
but several claims predate work already present on this branch and must not be
copied into the final record:

| Draft claim                                                          | Verified branch state                                                                                                                                                                                                                                                                                                                                                        | Decision-record treatment                                                                                                                                                                                                                           |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Attach/frame streaming is private and not pinned.                    | Warm attach is a public `sessions.attach` catalog row (`crates/thegn-core/src/capability.rs:193-203`), route/API entry (`crates/thegn-svc/src/control/routes.rs:148-163`), and documented control-v1 route (`docs/api/control-v1.json:818-821`). The codec is versioned by `PROTO_VERSION` and rejects unknown tags (`crates/thegn-core/src/control_wire.rs:21-35,304-395`). | Say that attach exists and is versioned, but that the binary frame variants are not fully represented by the generated JSON schema. A future GUI contract still needs an explicit client-facing schema/compatibility policy; do not invent it here. |
| Only one UI client is supported; multi-client attach is future work. | `AttachKind::Observer` and `Interactive` are part of the wire contract (`crates/thegn-svc/src/control/mod.rs:189-197`); the actor replaces a stale client id, retains multiple subscribers, and only interactive clients resize (`crates/thegn-host/src/daemon/session.rs:610-645,655-678`).                                                                                 | Mark multi-subscriber attach as already satisfied in the current substrate. Future work is conflict policy and a public GUI compatibility contract, not basic second-client existence.                                                              |
| `GET /pair` is absent and there is no CORS story.                    | The control state carries exact-origin `cors_origins` (`crates/thegn-svc/src/control/http.rs:38-53`), and a self-contained pairing page is served at `pair_page` (`crates/thegn-svc/src/control/http.rs:276-297`).                                                                                                                                                           | Do not repeat the stale 404/no-CORS claim. TLS is still not provided by v1: `docs/api/control-v1.json:17-20` requires a trusted network or tunnel for plaintext TCP.                                                                                |
| A dependency ban and boundary edits belong in this change.           | The draft proposes edits to `deny.toml` and `crates/thegn-core/tests/crate_boundaries.rs`, but THE-40 explicitly requires no code and only a decision record. Current bans are limited to historical `vt100`/`russh` (`deny.toml:80-92`); the boundary test is the owner table (`crates/thegn-core/tests/crate_boundaries.rs:19-66`).                                        | Cut those implementation edits. Record the future boundary rule here and make the future frontend crate own its GUI substrate through a separate, reviewed change.                                                                                  |
| Add a new AP roadmap row as part of this issue.                      | AP currently contains the long-horizon bets (`tasks.md:1251-1259`); J 127 is the existing optional web-terminal placeholder (`tasks.md:601-617`), and K is adaptive/mobile UI (`tasks.md:619-631`).                                                                                                                                                                          | Do not mutate the roadmap in this docs-only architect commit. The coder chunk may sync the decision record into the archived openspec change; roadmap placement is a separate product-audit choice.                                                 |

The branch `tg/the-34-herdr-api` is the coordination point for event filters
and the `events.subscribe` projection. Its architect artifact says filters are
per-connection, lag signaling is opt-in, the existing broadcast remains the
source, and the proposed `State` frame was cut because `SessionInfo` and
`WorktreeInfo` are svc-owned (`tg/the-34-herdr-api:.thegn/pipeline/THE-34/architect/design.md`).
THE-40 depends on that direction; it must not design a competing event
subscription protocol or reintroduce the rejected state frame.

## What a GUI needs from the substrate

The relevant substrate is the one capability catalog plus the control-v1
contract. The catalog is authoritative: it has one row per verb, a scope, and
surface projections (`crates/thegn-core/src/capability.rs:1-15,128-150,183-203`),
while the architecture document requires HTTP/WS/SSE, gRPC, CLI, MCP, and
plugin doors to project that same catalog (`docs/ARCHITECTURE.md:151-177`). A
GUI must therefore be a client of existing rows and a future GUI surface must
not receive a GUI-only verb.

### 1. Session attach and stream

Already present:

- `sessions.list` exposes session identity, worktree hint, geometry, attached
  clients, lease and exit state through `SessionInfo`
  (`crates/thegn-svc/src/control/mod.rs:51-101`). The corresponding schema is
  pinned in `docs/api/control-v1.json:431-530`.
- `sessions.attach` is a write-scoped WebSocket attach with `client_id`,
  `rows`, `cols`, `observer`, and `history` query parameters
  (`crates/thegn-svc/src/control/client.rs:525-605`). `observer` is the
  read-mostly choice; an interactive client may resize, while observers do not.
- The initial stream is `Hello`, then a `PaneSnapshot`, then sequenced
  `PaneDelta` frames. Snapshot sequence plus the first delta sequence is the
  resynchronization contract (`crates/thegn-svc/src/control/mod.rs:199-207`;
  `crates/thegn-core/src/control_wire.rs:103-145,218-395`). The daemon’s
  subscriber is bounded and sends a fresh snapshot after lag rather than
  blocking the PTY (`crates/thegn-host/src/daemon/session.rs:563-603`).
- `sessions.input`, `sessions.resize`, `sessions.detach`, `sessions.snapshot`,
  and `sessions.kill` are separate catalog rows
  (`crates/thegn-core/src/capability.rs:204-233`) and the routes are pinned in
  the control schema. A GUI should call these through the typed client/API,
  not reach into PTYs or the database.

Still needed before a production GUI client:

1. Publish the binary `EventFrame` variant/header/sequence contract as a
   first-class, compatibility-tested GUI client contract. `control-v1.json`
   pins the route and JSON request/response types, but the binary frame codec
   itself is implemented by `EventFrame::encode`/`EventDecoder`, not fully
   described in that JSON schema (`crates/thegn-core/src/control_wire.rs:218-395`).
2. Define reconnect and version-skew behavior for a GUI-owned terminal
   emulator: when to request `history=false`, how to discard stale deltas, how
   to react to a lag/resync snapshot, and which protocol features are required.
   The current typed client has a 10-second greeting/version guard
   (`crates/thegn-svc/src/control/client.rs:611-682`), but that is not yet a
   complete GUI compatibility policy.
3. Define ownership of terminal geometry when an interactive TUI and GUI are
   both present. Today “last interactive writer wins” is the actor behavior
   (`crates/thegn-host/src/daemon/session.rs:618-621`); a GUI product needs a
   deliberate UX around observer mode, resize contention, and input handoff.
4. Select and test a substrate-free client-side terminal emulator. The GUI
   process may own a terminal-emulation implementation, but no emulator or
   windowing dependency may leak into `thegn-core`; the architecture boundary
   explicitly keeps substrate crates out of core (`docs/ARCHITECTURE.md:1-35`).

The current pane stream is enough to prototype a single observer cell view in
a future follow-up. It is not enough to claim that a stable, independently
versioned GUI product contract is complete.

### 2. Layout and state

Already present:

- `worktrees.list` exposes actionable worktree metadata, including path,
  branch, repository root, location, and creation time
  (`crates/thegn-svc/src/control/mod.rs:35-49`; `docs/api/control-v1.json:733-755`).
- Session metadata includes geometry, lease state, process state, and recording
  status. The DB remains a cache/resurrection layer, not the source of truth
  (`docs/ARCHITECTURE.md:230-235`).
- `events.subscribe` exists on HTTP, gRPC, and plugin surfaces and is read-only
  in the catalog (`crates/thegn-core/src/capability.rs:362-368`). It publishes
  `Activity`, `Lease`, `Pairing`, `Sessions`, and `SessionExit` frames
  (`crates/thegn-core/src/control_wire.rs:123-145`).

Missing for native chrome:

- There is no catalog row or control-v1 resource for the compositor’s
  `CenterTree`, tabs, sidebar, panel, statusbar, overlays, focus, keymap, or
  hit targets. The control service says plainly that “the compositor’s tab/pane
  layout stays client-side; the daemon’s registry is flat”
  (`crates/thegn-svc/src/control/mod.rs:51-53`).
- There is no serializable chrome view model. The current TUI composes chrome
  into terminal cells under the host render path; a widget GUI would need
  stable component identity, content, state, actions, and geometry semantics.
- There is no server-side layout mutation contract. Even `sessions.split`
  currently opens a sibling session and leaves full in-layout placement for a
  future server-side layout (`crates/thegn-svc/src/control/mod.rs:631-643`).
- THE-34’s proposed event filters and opt-in lag signal will help a monitor
  consume only relevant state, but they do not turn the feed into a layout
  snapshot or replay journal. Follow THE-34’s explicit resync decision rather
  than adding a second state feed here.

Therefore a GUI that only renders terminal cells can proceed without native
chrome data, while a GUI that re-renders sidebar/panels cannot. The latter is
blocked on a separately designed component/view-model contract (the adjacent
THE-43 `add-ui-component-contract` work), plus catalog rows for any new
actions.

### 3. Event subscription

Already present:

- `events.subscribe` is one catalog row, read-scoped by `required_scope`, and
  routed at `/v1/events` and `/v1/events/sse`
  (`crates/thegn-core/src/capability.rs:362-368`; `crates/thegn-svc/src/control/routes.rs:66-69,161`).
- WebSocket and SSE pumps subscribe to the daemon broadcast off the render
  loop (`crates/thegn-svc/src/control/http.rs:1406-1468`); gRPC mirrors the
  feed (`crates/thegn-svc/src/control/grpc.rs:627-659`).
- The current stream’s `Hello` carries protocol version and granted scopes;
  TCP serve mode requires a bearer token while local Unix behavior is governed
  by `local_admin` (`docs/api/control-v1.json:1-20`; `crates/thegn-svc/src/control/auth.rs:16-43`).
- `thegn serve` already has pairing, one-time code redemption, scoped tokens,
  a pairing page, and exact-origin CORS configuration. Those are substrate
  inputs, not permission to add a browser-specific auth path.

Still needed for a GUI consumer:

1. Consume THE-34’s canonical event-kind/filter vocabulary and lag behavior;
   do not create GUI-specific query parameters or a second subscription row.
2. Add a machine-readable state bootstrap/resync contract only if a concrete
   consumer demonstrates that `sessions.list`/`worktrees.list` plus the
   `Sessions` poke are insufficient. THE-34 deliberately cuts its proposed
   `State` frame, so THE-40 does not preempt that decision.
3. Define whether a GUI needs the monitor feed, a session attach stream, or
   both. Pane bytes belong on `sessions.attach`, not the broadcast monitor
   feed; the current client documentation makes that split explicit
   (`crates/thegn-svc/src/control/client.rs:470-477`).

## Candidate shapes

| Candidate                                                            | Benefits                                                                                                                                                                                                                                                                                                                                  | Costs and risks                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | Judgment                                                                                                                                                                              |
| -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1. GPU terminal-embedding shell, such as Tauri/egui, hosting the TUI | Can offer a window, launcher integration, GPU text, and a fast visual proof while reusing the existing TUI’s chrome. App presence is already covered by the macOS launcher decision: it launches the TUI in the user’s terminal rather than becoming a GUI (`openspec/changes/archive/2026-08-23-add-macos-app-launcher/design.md:1-18`). | If it hosts the TUI process, it adds a terminal/window wrapper, lifecycle and input forwarding, and another terminal boundary without adding semantic chrome. If it embeds a terminal emulator, it duplicates terminal fidelity and pays toolkit distribution/security cost. Tauri/webview makes a poor native terminal and is particularly awkward over a plaintext v1 serve listener. An egui/winit/wgpu feature in `thegn-host` would violate the single compositor and substrate ownership rules. | Reject as the product lane. A wrapper is app presence, not a native GUI; a host-integrated prototype is explicitly out of bounds.                                                     |
| 2. Native chrome re-render over the control API                      | A separate frontend can own a real window and GPU renderer, reuse daemon PTY ownership, preserve warm attach, use observer/input/resize policy, and invoke only catalog capabilities. It can eventually provide OS-native IME, font fallback, drag/drop, accessibility, and image handling.                                               | Highest design cost: terminal emulation, binary stream compatibility, reconnect/resync, layout and chrome view model, input/resize contention, packaging, and security. Re-rendering chrome before THE-43 would fork the product and create a second source of truth.                                                                                                                                                                                                                                 | Preferred future shape, but only after the attach contract is published and a real demand signal exists. Start with cell rendering; add native chrome only after a view model exists. |
| 3. Web client for `thegn serve`                                      | Fast UI iteration, remote access, no native package per client, and a natural consumer of the existing pairing page, CORS policy, HTTP/SSE/WS, and scoped `read`/`write` tokens. Useful for mobile/remote monitoring even if no native GUI ships.                                                                                         | Browser terminal emulation and binary/WebSocket handling add fidelity and accessibility work. v1 TCP is plaintext and requires a trusted network or tunnel (`docs/api/control-v1.json:17-20`); public deployment needs the separate TLS/auth hardening lane. CORS, token handling, clipboard, IME, mobile input, and reconnect are security/product work. It is a web-terminal lane, not proof that a native GUI is warranted.                                                                        | Defer to THE-39 / remote-access work. Reuse the same catalog and published stream contract if pursued; do not bundle it into Tauri to disguise the cost.                              |

Recommendation: no candidate is implemented in THE-40. When reopened, candidate
2 is the architectural lane. Candidate 3 may proceed independently as remote
access. Candidate 1 is limited to the already-shipped app-wrapper tier.

## Invariants for every future frontend

1. **0% idle.** The shell still blocks on `poll_input(None)` when idle; the
   architecture’s timing and wake rules are explicit (`CLAUDE.md` event-loop
   invariant; `docs/ARCHITECTURE.md:80-84`). A separate frontend must wait on
   its transport/event loop, not cause a host tick or polling timeout. Any
   in-process producer remains channel + `TerminalWaker`, and any daemon work
   stays off the compositor loop.
2. **The shell never depends on the GUI.** `thegn-host` remains the reference
   frontend and must launch, render, detach, and degrade without a GUI process.
   `[daemon] enabled = false` remains a valid fully in-process pane mode
   (`CLAUDE.md` architecture overview). GUI failure, absence, disconnect, or
   upgrade cannot prevent the TUI or CLI from operating.
3. **One catalog.** GUI actions use `thegn_core::capability::CATALOG`, its
   `required_scope`, and the existing control projections. No GUI-only
   capability, ad hoc route, policy table, token type, or direct DB/PTY access.
   A streaming capability must be explicitly represented in the catalog’s
   surface set; `api call` remains request/response and does not pretend to
   call a stream (`crates/thegn-svc/src/control/routes.rs:142-147`).
4. **Degrade at the edges.** Core/domain state stays substrate-free and
   unit-tested; transport, GUI toolkit, font shaping, GPU, and OS integration
   stay in the future client/leaf boundary. The current renderer composes
   truecolor + Unicode and degrades at wire/glyph chokepoints
   (`docs/ARCHITECTURE.md:86-99`). A GUI may have richer output, but must not
   force those substrates into `thegn-core` or the TUI.
5. **Provider seams, not vendors.** If the future client needs a selectable
   renderer, transport, or terminal backend, use a seam with capability
   negotiation; do not hard-code a toolkit/vendor into shared code. This is
   consistent with the object-safe seam rule (`docs/ARCHITECTURE.md:110-149`).
6. **No god-file growth.** A future client is a new crate/modules with a thin
   transport adapter. Do not add GUI branches to `run.rs`, `chrome.rs`,
   `main.rs`, or the daemon actor. The current control client already keeps
   transport logic in its sibling module (`crates/thegn-svc/src/control/client.rs:1-15`).
7. **No new config by implication.** A GUI preference or endpoint is not free:
   every new key must be in the Rust schema, `config/config.toml.example`,
   generated config-reference help, env-overlay coverage, and related ratchets
   in the same implementation chunk (`docs/ARCHITECTURE.md:199-214`). THE-40
   adds none.
8. **Security follows the existing edge.** Pairing and scoped bearer tokens
   remain the authority; secrets are never logged or persisted in plaintext.
   A GUI gets no broader scope than another client and must handle missing
   daemon, denied scope, expired pairing, lag, and protocol mismatch honestly.
9. **No new wake, DB, or migration contract in the decision.** A future client
   may read the existing state through control APIs; it must not open the live
   SQLite state DB or make the GUI the source of truth. Any state-schema change
   is a separate design with migration tests.

## Smallest first slice (file as a follow-up, not in THE-40)

The smallest useful follow-up is not a GUI window. File a follow-up titled:

> **THE-40-F1 — Publish the observer cell-client contract**

Its scope should be deliberately narrow:

- consume `sessions.list` and `worktrees.list` with `read` scope;
- attach one session as `observer`, decode `Hello`/`PaneSnapshot`/`PaneDelta`,
  render a single terminal cell grid, and detach cleanly;
- consume the post-THE-34 `events.subscribe` filter/lag contract only for
  session/activity resync hints;
- specify sequence, reconnect, version mismatch, bounded lag, and geometry
  behavior in a pinned client fixture/schema;
- use an isolated test daemon/fixture and no new GUI toolkit, chrome model,
  config key, capability, database migration, or web surface.

The follow-up is a contract/fixture spike that can answer whether the stream is
pleasant and stable to consume. It intentionally does not choose egui/Tauri,
add native chrome, or solve layout. Reopen the larger candidate-2 decision only
after this slice, `add-runtime-session-split`’s finalized ownership semantics,
and THE-43’s component contract are available, and after someone documents a
need terminal emulators do not meet (for example IME, OS font fallback,
drag-and-drop, image-heavy review, or a platform with poor terminal options).

## Ratchet and validation disposition

This architect change has no implementation files, so no ratchet changes are
valid or necessary:

- no config key means `config/config.toml.example`, env-overlay, config enum,
  and config-help ratchets stay unchanged;
- no action/key means completion-slot and help ratchets stay unchanged;
- no capability or route means catalog, surface-gap, control-schema, and help
  ratchets stay unchanged;
- no crate/dependency means boundary and dependency-ban ratchets stay
  unchanged;
- no render, producer, thread, or DB change means idle, render, QoS, ignored
  result, and migration ratchets stay unchanged.

The future implementation chunks must update the relevant ratchets in the same
chunk. A GUI toolkit must be introduced as a separately owned frontend crate
with an explicit boundary decision; THE-40 does not pre-authorize a dependency
ban that would also block that sanctioned client.

## Files and follow-up ownership

The only artifacts delivered by this architect lane are this design and one
independent documentation chunk specification. The coder chunk owns the final
decision-record doc plus openspec synchronization/archive. It must preserve
the evidence corrections above, keep the active capability/architecture code
untouched, and leave no implementation or new config key behind.
