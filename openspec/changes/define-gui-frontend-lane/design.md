# Design — the GUI frontend lane (a decision record)

This change is deliberately a **not-now with teeth**: it ships one mechanical
gate and a recorded decision, not a GUI. The design section therefore argues
the judgement rather than an implementation.

## Why the model/view split makes a second frontend conceivable — and why

## "conceivable" is not "cheap"

What already points the right way:

- **The daemon owns PTYs** (`control-plane`): sessions survive UI detach;
  warm-reattach replays emulator snapshot + deltas. A frontend is already, in
  a real sense, a client of a session-owning server.
- **One capability catalog**: every external door (HTTP/gRPC/CLI/MCP/plugin)
  projects `thegn_core::capability::CATALOG` with `required_scope(verb)`. A
  GUI's _actions_ need no new policy surface — they are catalog calls.
- **`thegn serve` + pairing**: thin clients pair via a URL, get scoped
  hashed-stored tokens. The auth model for a remote frontend exists.
- **`AppTile` (tg-kit)**: the embedded-app contract already proves the host
  can drive a foreign render surface (ratatui buffer, ChangeHook waker) under
  the 0%-idle discipline.

What does not exist, and is load-bearing:

- **The frame stream is private.** The UI process composes chrome in-process
  (`render_tab`, termwiz `Surface`) and talks to the daemon over an internal
  socket protocol. No version-pinned public contract describes "attach and
  receive a session's frames" the way `docs/api/control-v1.json` pins control
  or `plugin-api-<v>.json` pins plugins. A GUI built today would freeze an
  internal protocol by accident.
- **One UI client at a time.** The session model that would let a GUI attach
  _beside_ the TUI (or replace it live) is the multi-client attach work
  scoped by `add-runtime-session-split`. Building a GUI first inverts the
  dependency.
- **Chrome is cells, not a view-model.** Sidebar/panel/bars are composed
  directly into the cell grid. A widget GUI (lane 3) needs chrome elements as
  data — id, content, hit-targets, state — which is exactly the contract
  `add-ui-component-contract` (THE-43) introduces for its own reasons.
  Until then a GUI either re-renders cells (lane 2 — fine, but then it is a
  terminal emulator) or forks the chrome (a second product).

## The three lanes, judged

| Lane                             | What it is                                                                                          | Verdict                                                                                                                        |
| -------------------------------- | --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| 1. App presence                  | `.app` bundle / `.desktop`, user's terminal hosts the TUI                                           | **Shipped** (`macos-app-launcher`); keep investing here                                                                        |
| 2. GPU thin-client cell renderer | separate binary, owns a window, renders the cell grid of a daemon session; chrome fidelity for free | **Not now** — gated on `add-runtime-session-split` + a pinned attach wire; the only lane worth building later                  |
| 3. Native widget GUI             | egui/gpui chrome + embedded terminal grid                                                           | **Not now, and not next** — needs THE-43's component model matured into a serializable view-model; otherwise a parallel chrome |

Lane 2's later shape, for the record (not scoped here): a `thegn-gui` client
crate outside `thegn-host`, speaking (a) the pinned attach/frame wire for the
grid and (b) the existing control API for actions, holding a pairing-scoped
token like any thin client. Everything it needs from the host is then an
additive wire publication, not a refactor.

## Alternatives considered

- **Feature-flagged egui inside `thegn-host`** ("just add a window"): rejected.
  It forks the render path (two backends behind `render_plan`), drags winit/
  wgpu into the compositor crate's dependency closure, and violates the
  degrade-at-the-edges model (chrome would need parallel non-cell draw code).
  The gate added by this change makes this rejection mechanical.
- **Tauri/webview wrapper**: rejected — a webview terminal (xterm.js) demotes
  fidelity and performance below any real terminal emulator the user already
  has, while paying full app-distribution cost (signing, updates). THE-39's
  web surface, if it comes, comes as a thin client, not a bundled webview.
- **Do nothing (no gate, just prose)**: rejected — every invariant in this
  repo that survived did so as a ratchet or test, not prose (CLAUDE.md's
  ratchet history). A two-line `deny.toml` + boundary-test addition is the
  cheapest possible enforcement.
- **Scope the attach wire now** so lane 2 is "ready": rejected — it would
  speculate on the session model `add-runtime-session-split` is actively
  designing, and a wrong pin is worse than none (pinned wires here are
  snapshot-tested; churning one is expensive).

## Reopen criteria

Revisit THE-40 (promote lane 2 to a real change) when **all** of:

1. `add-runtime-session-split` has landed daemon-owned session state with
   multi-client attach semantics.
2. The attach/frame stream is published as a version-pinned schema
   (`docs/api/…`) with a snapshot test, per house style.
3. `add-ui-component-contract` has landed (so statusbar/panel content a GUI
   might want as _data_ rather than cells has a contract to ride).
4. A documented demand signal a terminal emulator cannot meet: IME
   composition, OS-level font fallback, drag-and-drop, image-heavy review
   flows, or an OS where good terminal emulators are scarce (the Windows
   port's console story may become this signal).

## Render / event-loop impact

None. No damage channel is touched; no wake path is added. The gate is a
test-time mechanism only.

## Security

- **No new surface.** This change adds no socket, no route, no token kind.
- **Blast-radius pre-commitment:** by forcing any future GUI to be a thin
  client, its entire write surface is the already-scoped control API —
  pairing-issued tokens, `required_scope(verb)`, hashed storage in
  `pairings`. A GUI can never acquire in-process authority (direct DB or PTY
  handles) because the dependency gate keeps it out of `thegn-host`.
- The deny-list also protects against supply-chain drift: a GUI toolkit
  arriving as a transitive dependency of some utility crate fails
  `just deps-audit` loudly instead of silently linking a window stack into
  the compositor.

## Open questions

- Whether lane 2's client should reuse an existing terminal-emulator core
  (alacritty_terminal is currently owner-pinned to `thegn-host`'s boundary
  list; wezterm's term crate is another candidate) — decidable when the lane
  reopens; the ban list deliberately does not include terminal-emulator cores.
- Whether group AP is the right roadmap home versus a new group — left to the
  audit phase that normalizes across units.
