# Expand the media surfaces — docked now-playing, optional visualization, Spotify posture

Linear: THE-42

## Why

THE-42 asks for three things — "optional visualization, spotify support, move
it to a dock attached panel like the others" — against a media layer that is
already far more built than the issue assumes. What exists today (`[media]`,
roadmap AM 476): a dedicated `thegn-media` crate with per-OS backends behind
one object-safe `MediaBackend` seam — native MPRIS (zbus, push signals) +
`playerctl` CLI fallback on Linux, native MPD, mpv JSON-IPC, SMTC on Windows,
AppleScript/MediaRemote on macOS, `jellyfin` reserved — plus a statusbar badge,
a System ▸ Media panel section, and a full Now-Playing overlay (`Alt-m`: cover
art, scrubber, transport, queue, chapters). Spotify's desktop client (and
spotifyd, and ncspot/spotify-player) is already controlled through MPRIS/SMTC/
AppleScript today. Backends are already seam kinds with `thegn doctor` probes.

The real deltas are: (1) the Now-Playing overlay is a **centered modal**
(`Anchor::Center`), unlike the other chrome popups (calendar, detail views)
which dock beneath the masthead/statusbar element that owns them; (2) there is
**no visualization**; (3) Spotify has no first-class Web-API posture — and
whether it should is a genuine architecture question, argued in design.md.

## What Changes

- **Dock the Now-Playing popup.** Opening via the statusbar media badge (click)
  or `Alt-m` anchors the popup adjacent to the media badge, following the
  calendar-popup convention (`Placement::near`-style, opening upward from the
  statusbar), instead of a centered modal. Interior behavior (art, scrubber,
  queue, keys) is unchanged. When the badge is not currently rendered (nothing
  active, or the widget removed from `[bars]`), the popup falls back to a
  corner anchor near the statusbar edge — never a dead keybind. Small
  terminals degrade exactly as today (art drops first).
- **Optional audio visualization, `[media.viz]`** — off by default, zero cost
  when off or when the popup is closed. A spectrum-bars strip inside the
  Now-Playing popup, driven by a **visualizer provider seam**: kind `cava`
  (implemented — spawn the user-installed `cava` binary in raw ASCII output
  mode, parse frames off-thread, pulse the waker; killed on popup close) and
  kind `native` (**reserved** — in-process capture/FFT). Frames flow only
  while the popup is open AND playback is active; cadence is capped
  (`fps`, default 15). A missing `cava` binary means the strip is silently
  absent and `thegn doctor` says why. Honest verdict on cost vs charm is in
  design.md — this is a charm feature and is scoped so it can never tax the
  0%-idle contract or anyone who doesn't opt in.
- **Spotify: posture, not an OAuth client (yet).** `spotify` becomes a
  **reserved** `[media] backend` kind (config accepts it, doctor reports
  `reserved`), reserved for a future Web-API provider (library/playlist
  search, Spotify Connect device transfer) with PKCE OAuth and SecretRef
  token custody. Today's answer — desktop client or spotifyd over MPRIS
  (Linux), SMTC (Windows), AppleScript (macOS) — is documented as a recipe in
  `docs/help/media.md`. The argument for reserving rather than implementing
  is in design.md.
- **Docs/help:** `docs/help/media.md` gains the viz keys/config, the docked
  popup description, and the spotifyd recipe (help-prose ratchet).

## Non-goals

- **An OAuth Spotify Web-API provider.** Reserved, argued, not built (see
  design.md — token-refresh subsystem, Premium gating, Spotify's Nov-2024 API
  restrictions).
- **In-process audio capture.** `native` viz is reserved; no PipeWire/cpal/FFT
  dependencies enter the tree in this change. openmeters (a Linux-only
  PipeWire GUI on iced/wgpu/Vulkan) is inspiration for meter types only.
- **Visualization outside the popup.** No statusbar/panel-section animation —
  a perpetually animating badge is a standing render tax and is exactly what
  the render-plan economy exists to prevent.
- **New media backends** (termusic is a standalone player, not an integration
  target; rmpc is an MPD client already covered by the native MPD backend).

## Impact

- Roadmap: extends **AM 476** (music tile — landed); THE-42 has no dedicated
  row yet (the audit phase wires it into group AM).
- Specs: new `media` capability spec (the shipped `[media]` feature predates
  the spec corpus; this change adds only the new behaviors as ADDED
  requirements — a full retro-spec of the existing feature is separate work).
- Code (indicative): `thegn-host/src/media_overlay.rs` (anchor),
  `thegn-host/src/media_viz.rs` (new: cava spawn/parse/lifecycle),
  `thegn-core/src/config_media.rs` (`[media.viz]`, `spotify` reserved kind),
  `thegn-svc/src/seam/registry.rs` (viz probe, spotify reserved probe),
  `thegn-host/src/e2e_freeze.rs` (pin viz frames under `THEGN_E2E`).
- Capability catalog: **no new rows** — docking and viz are UI-only; the
  reserved spotify kind exposes no operations. If the Web-API provider lands
  later, its library/search ops become catalog rows in that change.
- No SQLite schema change. No new Rust dependencies.
- e2e: the Now-Playing anchor change alters frames — re-record affected
  baselines with `just e2e-update`.
- In-flight overlap: none — `add-drawer-tool-registry` generalizes the bottom
  drawer (a different surface; the media popup is chrome, not a drawer
  occupant). `add-calendar-and-world-clock` is precedent for the docked-popup
  convention, not a dependency.
