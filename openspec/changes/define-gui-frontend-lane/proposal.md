# Native GUI — pick the lane, pin it, and deliberately not build it yet

Linear: THE-40

## Why

THE-40 is two words: "native GUI". Before anyone burns a quarter on a window
toolkit, the direction needs an honest answer, because "native GUI" reads three
different ways against this architecture — and they differ by two orders of
magnitude in cost:

1. **Native app presence** — Dock/Spotlight/launcher integration, a dedicated
   window. This is largely **shipped**: `macos-app-launcher` generates a
   `thegn.app` that resolves a terminal emulator and the binary by absolute
   path; Linux gets a `.desktop` entry + hicolor icon. Remaining work here is
   packaging polish, already owned by that spec.
2. **A GPU-native thin-client window** — a separate frontend process that owns
   a window and renders a session's **terminal cell grid**, attached through
   the daemon. Conceivable precisely because of choices already made: the
   daemon owns PTYs (`control-plane`), warm-reattach replays snapshot +
   deltas, `thegn serve` pairs thin clients with scoped tokens, and every
   externally invokable operation is a `capability::CATALOG` projection. What
   is _missing_ is equally concrete: the attach/frame stream is a private
   contract between thegn's own UI and its daemon, not a pinned public wire; a
   session model that tolerates a second concurrent client is in-flight
   elsewhere (`add-runtime-session-split`); and the serve surface has no
   sanctioned browser/remote story yet (`complete-control-surface-coverage`,
   THE-39, found `GET /pair` 404s and no CORS/TLS story).
3. **A native widget GUI** — sidebar/panel/tabs as real widgets (egui/gpui/
   iced), terminal grid only in the center. This requires chrome to exist as a
   _serializable view-model_ rather than cells painted by ~free functions in
   `chrome.rs`. That prerequisite is exactly what THE-43's component contract
   (`add-ui-component-contract`) starts building. Without it, a widget GUI is
   a parallel reimplementation of the entire chrome — a second product.

The honest judgement: **do not build any GUI now.** Every prerequisite for
lane 2 — the only lane that could responsibly be built — is already in-flight
under other changes, and building ahead of them means inventing a private
frames-over-wire protocol that `add-runtime-session-split` would then
obsolete. What THE-40 _should_ do today is cheap and direction-setting: pin
the lane architecturally so that a future GUI is forced to be the right kind
of thing (a thin client of the daemon, never a second in-process render
backend), and record the criteria that reopen the decision.

## What Changes

- **A new architecture gate: graphical frontends are thin clients.** GUI
  toolkits and window-system substrates (`egui`, `eframe`, `iced`, `winit`,
  `wgpu`, `gpui`, `tauri`, `slint`, `druid`) are added to the crate-boundary
  owner test with **no owner crate**, and to `deny.toml` bans — so a GUI can
  only ever appear as a _new_ client crate speaking the pinned control/attach
  wire, added deliberately with its own boundary entry, never as a feature
  flag inside `thegn-host`. This is the same mechanism that already bans
  `vt100`/`russh` and pins `tokio`/`termwiz` to owner crates.
- **The lane is recorded** in `docs/ARCHITECTURE.md` (frontend note alongside
  §6's external-doors story): the terminal UI is the reference frontend; any
  graphical frontend attaches through the daemon like every other thin
  client; a capability is never GUI-only.
- **Reopen criteria are recorded** in this change's design.md (the decision
  record): (a) `add-runtime-session-split` lands the daemon-owned session
  model with multi-client attach; (b) the attach/frame stream is published as
  a version-pinned wire contract (like `control-v1.json` /
  `plugin-api-<v>.json`); (c) `add-ui-component-contract` lands chrome as
  declared components; (d) a demand signal terminal emulators cannot meet
  (IME, font fallback, image-heavy workflows) is documented. Until then,
  "native GUI" energy goes to the app-wrapper tier that exists.
- **No GUI code, no new capability, no new config keys.**

## Impact

- Roadmap: no existing item says "native GUI". Group **K** (adaptive UI) and
  **J 127** (auth-gated web terminal) are adjacent but distinct; this adds a
  new item to group **AP** (long-horizon bets): "Native GUI frontend — gated
  on runtime-session-split + pinned attach wire + component contract; thin
  client only" so the roadmap carries the decision.
- Specs: `architecture-gates` — ADDED requirement (frontend substrates are
  banned outside a declared frontend client crate).
- In-flight changes this defers to (and must not re-scope):
  `add-runtime-session-split` (daemon session model, multi-client attach),
  `complete-control-surface-coverage` (THE-39 — pairing page, serve
  TLS/CORS, catalog coverage; the control-plane surface a GUI would ride),
  `add-ui-component-contract` (THE-43 — the chrome view-model prerequisite),
  `macos-app-launcher` spec (the shipped app-presence tier).
- Code: `deny.toml`, `crates/thegn-core/tests/crate_boundaries.rs`,
  `docs/ARCHITECTURE.md`. Nothing else.

## Non-goals

- **Building a GUI** — any lane, any toolkit, including a "small egui
  prototype": a prototype inside `thegn-host` is precisely the second render
  backend the gate exists to prevent.
- **Designing the attach/frame wire.** That contract belongs to
  `add-runtime-session-split` + the control-plane spec when multi-client
  attach is real; pinning it prematurely here would be speculation.
- **A web frontend.** THE-39 (`complete-control-surface-coverage`) owns the
  browser/pairing surface question.
- **Electron/Tauri wrappers.** A webview hosting a terminal emulator is the
  worst of both lanes (native cost, terminal fidelity loss); the generated
  `.app` + user's own terminal already covers app presence.
