# THE-40: Native GUI frontend lane

Linear: THE-40; decision type: architecture / documentation only
Status: not now; preserve a future thin-client lane

## Decision

Do not build a GUI in THE-40. A future native GUI is additive: it is a
separate frontend that consumes the daemon and control API, never a second
renderer inside `thegn-host`, a second capability registry, or a runtime
dependency of the shell.

When this decision is reopened, the preferred shape is candidate 2: a GPU
terminal-cell client. It may grow native chrome only after the substrate can
publish a stable, serializable chrome view model. A web client can consume the
same substrate, but belongs to the separate remote-access lane.

This record adds no Rust, frontend, dependency, configuration, capability,
database, migration, wire, route, roadmap, or ratchet change.

## Verified substrate and corrections

The earlier OpenSpec draft correctly identified the architectural choice, but
several of its substrate assumptions predated work now on this branch.

| Area                      | Verified branch state                                                                                                                                                                               | Consequence for this decision                                                                                                                                                                                                 |
| ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Attach contract           | `sessions.attach` is a public catalog row and control-v1 route. `PROTO_VERSION` versions the codec and unknown frame tags are rejected.                                                             | Attach is real and versioned. The binary `EventFrame` variants, headers, and sequence behavior are not fully represented by the generated JSON schema, so an independently consumable compatibility contract is still needed. |
| Concurrent clients        | `AttachKind::Observer` and `Interactive` are on the wire. The daemon actor retains multiple subscribers, replaces only a stale matching client id, and permits resize only for interactive clients. | Basic observer/multi-subscriber attach already exists. Follow-up work concerns input/resize conflict policy and compatibility, not enabling a second client.                                                                  |
| Pairing and browser edges | The control service serves a self-contained pairing page and supports configured exact-origin CORS.                                                                                                 | Pairing and CORS are substrate already. Control-v1 TCP remains plaintext and requires a trusted network or tunnel; public web deployment remains separate security/product work.                                              |
| Layout and chrome         | The daemon registry is flat and the compositor keeps its pane tree, tabs, sidebar, panels, overlays, focus, keymap, hit targets, and chrome layout client-side.                                     | There is no server-side layout/chrome model for a native-widget frontend. Candidate 2 can begin as a cell renderer; semantic chrome must wait for a serializable view model.                                                  |
| Enforcement               | GUI toolkits are not currently banned by `deny.toml` or assigned by the crate-boundary owner table.                                                                                                 | THE-40 changes neither gate. A future frontend implementation must introduce its client crate and substrate ownership together in a separate reviewed change.                                                                 |

The relevant implementation evidence is in
`thegn_core::capability`, `thegn_core::control_wire`, the control routes and
typed client under `thegn-svc`, and the daemon session actor under
`thegn-host`. The control-v1 schema pins the attach route and its JSON
parameters, but the binary pane stream remains implemented in the codec rather
than fully described in that schema.

THE-34 is the coordination point for event filtering and lag vocabulary. Its
final contract makes filters per connection, makes lag signaling opt-in, keeps
the existing broadcast as the source, and does not introduce a `State` frame.
THE-40 consumes that contract; it does not design another
`events.subscribe` protocol or state-replay feed.

## What a graphical client needs

### Session attach and cells

Already available:

- `sessions.list` exposes session identity, worktree hint, geometry, attached
  clients, lease, recording, and exit state.
- `sessions.attach` accepts `client_id`, rows, columns, observer mode, and
  history selection. The stream begins with `Hello`, followed by a
  `PaneSnapshot` and sequenced `PaneDelta` frames.
- A bounded daemon subscriber recovers from lag with a fresh snapshot instead
  of blocking the PTY.
- Input, resize, detach, snapshot, and kill are separate catalog capabilities.
  A client uses those APIs rather than opening PTYs or the state database.

Still required before a production GUI:

1. Publish the binary frame variants, headers, sequencing, and compatibility
   behavior as a first-class, fixture-tested client contract.
2. Define reconnect and version-skew behavior, including stale-delta discard,
   lag/resnapshot handling, history selection, and required protocol features.
3. Define user-facing ownership of terminal geometry and input when an
   interactive TUI and GUI coexist. Current actor behavior is effectively the
   last interactive resize writer winning.
4. Select and test a client-side terminal emulator without leaking terminal,
   windowing, or GPU substrates into `thegn-core`.

The current stream is sufficient for a future observer cell-client spike. It
is not yet a complete compatibility promise for an independently released GUI.

### Layout, state, and chrome

`worktrees.list`, `sessions.list`, and the monitor feed expose useful domain
and lifecycle state. They do not expose the compositor's layout or chrome.
There is no catalog resource for `CenterTree`, tab ordering, sidebar/panel
content, statusbar, overlays, focus, keymap, or hit targets, and there is no
server-side mutation contract for those concepts. Even session splitting
creates a sibling session without specifying full compositor placement.

THE-34's filtered event stream can provide targeted change and lag hints, but
it is not a layout snapshot or replay journal. A native-widget GUI therefore
waits for a separately designed component/view-model contract, coordinated
with THE-43, and for catalog rows covering any new actions. Until that exists,
the cell grid is the only shared visual model.

### Events and remote access

`events.subscribe` is the one read-scoped catalog capability used by HTTP,
WebSocket/SSE, gRPC, and plugin projections. Pane bytes remain on
`sessions.attach`, not the monitor feed. A GUI needing both lifecycle hints and
pane cells consumes both existing roles rather than inventing a GUI-specific
subscription.

Pairing, one-time code redemption, scoped tokens, the pairing page, and
exact-origin CORS are available substrate. They do not make public web serving
complete: v1 TCP has no TLS and must remain behind a trusted network or tunnel.
That hardening and browser UX belong to THE-39/remote access, independently of
the native GUI choice.

## Candidate shapes

| Candidate                                                                                          | Benefits                                                                                                                                                                                             | Costs and risks                                                                                                                                                                                                                                                                                                       | Judgment                                                                                                                              |
| -------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| 1. GPU terminal-embedding shell, such as Tauri/egui, hosting the TUI                               | Provides a window, launcher integration, GPU text, and a quick visual proof while reusing cell chrome.                                                                                               | It adds another terminal/window lifecycle and input boundary without adding semantic chrome. Embedding it in `thegn-host` forks the renderer and dependency closure; a webview loses terminal fidelity while adding distribution and security cost. App presence is already covered by the generated native launcher. | Reject as the product lane. A wrapper is app presence, not the native GUI architecture.                                               |
| 2. Native client over the control API, beginning with cell rendering and later re-rendering chrome | A separate frontend can reuse daemon PTY ownership, warm attach, observer/input/resize policy, and catalog actions while owning native IME, font, accessibility, drag/drop, image, and GPU behavior. | It must solve terminal emulation, binary-stream compatibility, reconnect/resync, input and resize contention, packaging, and security. Native chrome before a view model would create a second source of truth.                                                                                                       | Preferred future shape. Start with a single observer cell grid; native chrome waits for a serializable model and demonstrated demand. |
| 3. Web client for `thegn serve`                                                                    | Fast iteration, remote/mobile access, and direct use of HTTP/SSE/WebSocket, pairing, CORS, and scoped tokens.                                                                                        | Browser terminal fidelity, reconnect, clipboard, IME, mobile input, token handling, and public TLS deployment are substantial work. It answers remote access rather than native GUI demand.                                                                                                                           | Defer to THE-39/remote-access work. Reuse the catalog and published cell-stream contract; do not bundle it into a native webview.     |

No candidate is implemented by THE-40. Candidate 2 is the lane to reopen;
candidate 3 can proceed independently; candidate 1 remains limited to the
existing app-wrapper tier.

## Reopen criteria

Revisit the candidate-2 product decision when all of the following are true:

1. THE-40-F1 has published and fixture-tested the observer cell-client
   contract, including reconnect, sequence, lag, geometry, and version-skew
   behavior.
2. Runtime-session ownership semantics are finalized for input and resize
   contention between observer and interactive clients.
3. THE-43's component contract has matured into a stable, serializable chrome
   view model before any native chrome is attempted.
4. A concrete demand signal is documented that good terminal emulators cannot
   meet, such as IME composition, OS font fallback, drag-and-drop,
   accessibility, image-heavy review, or a platform with poor terminal
   options.

## Invariants for every future frontend

1. **0% idle.** The shell still blocks on `poll_input(None)` when idle. A
   separate frontend waits on its own transport/event loop and must not induce
   a host tick or polling timeout. Host producers remain channel plus
   `TerminalWaker`; daemon work remains off the compositor loop.
2. **The shell never depends on the GUI.** `thegn-host` remains the reference
   frontend. It must launch, render, detach, and degrade without a GUI process;
   `[daemon] enabled = false` remains valid. GUI absence, failure, disconnect,
   or version skew cannot prevent TUI or CLI operation.
3. **One catalog.** GUI actions project
   `thegn_core::capability::CATALOG` and its scopes. There is no GUI-only
   capability, route, policy table, token type, or direct database/PTY access.
4. **Degrade at the edges.** Domain state remains substrate-free. Terminal
   emulation, transport, toolkit, GPU, font shaping, and OS integration stay
   at the future client/leaf boundary and out of `thegn-core`.
5. **Provider seams, not vendors.** Selectable rendering, transport, or
   terminal backends use capability-bearing seams rather than hard-coded
   shared-code vendor choices.
6. **No second compositor path.** The future client is a separate crate with
   a thin transport adapter. It does not add GUI branches to `run.rs`,
   `chrome.rs`, `main.rs`, or the daemon actor.
7. **No configuration by implication.** Any future key must land with schema,
   example, generated help, overlay coverage, and ratchets in its own
   implementation change. THE-40 adds none.
8. **Security follows the existing edge.** Pairing and scoped bearer tokens
   remain the authority. Secrets are neither logged nor stored in plaintext;
   missing daemon, denied scope, expired pairing, lag, and mismatch are normal
   client states.
9. **Git and the daemon remain authoritative.** A GUI does not open the live
   SQLite cache or become a new source of truth. Any migration is a separate
   design and implementation.

## THE-40-F1: smallest follow-up

File a separate follow-up named **THE-40-F1 — Publish the observer cell-client
contract**. Its deliberately narrow scope is to:

- consume `sessions.list` and `worktrees.list` with read scope;
- attach one session as an observer, decode
  `Hello`/`PaneSnapshot`/`PaneDelta`, render a single cell grid, and detach;
- consume THE-34's final event-filter/lag contract only for session/activity
  resynchronization hints;
- pin fixtures for sequence, reconnect, version mismatch, bounded lag, and
  geometry behavior; and
- use an isolated test daemon or fixture with no GUI toolkit, native chrome,
  config key, capability, database migration, or web surface.

The follow-up is a contract/fixture spike, not a GUI implementation. It does
not choose a toolkit or solve layout. A larger client proceeds only after that
evidence and the reopen criteria above exist.

## Ratchet disposition

No ratchet changes are valid in this decision-only issue. There is no new
config key, action, keybinding, capability, route, dependency, crate, render
path, producer, thread, database field, or migration. A future frontend change
must introduce its toolkit in a separately owned client crate and update every
applicable gate in the same implementation chunk; this record does not
pre-authorize those edits.
