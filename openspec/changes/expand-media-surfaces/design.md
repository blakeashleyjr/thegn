# Design — media surface expansion

## Current-state audit (what THE-42 already has)

| Ask                      | Today                                                                                                                                                                                                                                                       |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| "spotify support"        | Controlled via MPRIS (desktop client, spotifyd, ncspot, spotify-player), SMTC (Windows), AppleScript (macOS). Transport + metadata + art all work. `players_priority = ["spotify"]` prefers it among concurrent players.                                    |
| "dock attached panel"    | A System ▸ Media **panel section** exists; the full control surface is the `Alt-m` Now-Playing overlay — a centered modal (`Anchor::Center` in `media_overlay.rs`), unlike the calendar/detail popups which dock to their masthead/statusbar owner.         |
| "optional visualization" | None.                                                                                                                                                                                                                                                       |
| Seam posture             | `MediaBackendKind` is a proper seam-kind enum (`auto/none/mpris/mpv/mpd/smtc/applescript/jellyfin`), `jellyfin` reserved, probes in `thegn doctor` (`seam/registry.rs::media_probes`). Backends are object-safe `Box<dyn MediaBackend>` with push watchers. |

So this change is a surface delta, not a media stack.

## Docked popup

`layer.rs` already supports `Anchor::At { x, y }` with clamping; the media
badge's hit rect is known to the statusbar. Opening from the badge (click or
`Alt-m`) computes an origin adjacent to the badge rect, opening upward
(statusbar is the bottom row), mirroring the calendar popup's
`Placement::near`-from-masthead behavior. Fallback when the badge has no rect
this frame (nothing active, widget removed, statusbar hidden): a bottom-right
corner anchor — the chord must never go dead just because the badge is absent.

- **Render damage:** unchanged class — the popup is an overlay; opening,
  interior updates, and closing take the existing overlay path (`Full` frames
  while open, exactly as the centered modal does today). No new damage channel.
- **Wake path:** unchanged — the media watcher already pulses the waker on
  snapshot change.
- **Help context:** the popup keeps the existing media help mapping
  (`docs/help/media.md`); no new zone/panel context key.

## Visualization: cost vs charm, judged honestly

**The honest verdict:** a spectrum strip is pure charm — it carries zero
workflow information. It is worth having only if its cost is pinned to ~zero
for everyone who doesn't opt in and near-zero for those who do, in dependency
weight, idle CPU, and render pressure. That rules out the obvious
implementation and picks the boring one:

- **In-process capture + FFT (rejected for now → `native` reserved).**
  openmeters (linked from the issue) is the state of the art and is a
  Linux-only PipeWire GUI: pipewire-rs, RustFFT, iced/wgpu/Vulkan. Even a
  minimal in-process version means an audio-capture dependency
  (pipewire/cpal → C deps, per-OS permission surfaces), a capture thread that
  exists whether or not anything is drawn, and a hard portability split. That
  is a lot of tree-weight for charm.
- **`cava` as an external provider (chosen).** cava already solves capture +
  FFT on Linux (pulse/pipewire/alsa) and macOS (portaudio), is widely
  packaged, and has a raw output mode (data written to stdout/pipe as
  ASCII/binary frames) designed for embedding. thegn spawns it with a
  generated config, parses frames on a reader thread, sends them over a
  channel, and pulses the waker — the vendor binary stays inside the impl
  file per the seams rule. No new Rust dependencies; no capture code in-tree.
- **Fake visualization (rejected).** Bars derived from playback position or
  metadata are noise pretending to be signal; worse than nothing.

**Lifecycle pins the cost:**

- `[media.viz] enabled = false` default: no probe work, no spawn, no config
  keys consulted beyond the flag — zero overhead for non-users.
- The cava process is spawned when the Now-Playing popup **opens** while
  playback is active, and killed when the popup closes (or playback stops for
  a debounce interval). It never runs behind a closed popup — there is no
  standing capture process and no idle wake source.
- Frame cadence is capped by `fps` (default 15, clamped 5..=30): the reader
  thread coalesces to at most one waker pulse per frame budget. Each frame
  damages only the overlay region (the existing overlay repaint path).
- Under `THEGN_E2E=1`, viz frames are pinned (a fixed synthetic bar pattern)
  in `e2e_freeze.rs` so snapshots don't flap — the same rule as every other
  volatile chrome.
- Failure is silence: cava missing/crashing ⇒ the strip is absent, the popup
  is otherwise fully functional, `thegn doctor` explains
  (`viz: cava — unavailable (binary not on PATH)`).

Kinds: `auto` (cava if present, else none) | `cava` | `native` (reserved) |
`none`. `native` is the honest name for "someday, in-process capture" — if it
ever justifies its weight, it slots into the same seam without config churn.

## Spotify: MPRIS-via-spotifyd vs an OAuth Web-API provider

The argument, since THE-42 says "spotify support" without saying which kind:

**What MPRIS/SMTC/AppleScript already deliver:** play/pause/next/prev, seek,
volume, shuffle/loop, metadata, cover art — for the desktop client and for
headless spotifyd (Linux). That is 100% of what the _shell's_ media feature
does for every other player. There is no Spotify-shaped gap in transport.

**What a Web-API provider would add:** search, playlists/library browsing,
liked songs, and Spotify Connect device transfer — i.e. a media _browser_, not
a media _controller_. Costs:

- A user-created Spotify developer app + PKCE OAuth flow + a token-refresh
  loop — an entire credential subsystem (the same reason
  `add-calendar-and-world-clock` explicitly rejected OAuth calendar
  providers in favor of `.ics` URLs).
- Playback control via the Web API requires **Premium**; free accounts can
  browse but every transport call fails — a support-burden trap.
- Spotify **removed API endpoints for new apps in Nov 2024** (related
  artists, recommendations, and more) — building a provider on this API today
  is building on actively shrinking ground.
- The shell has no library-browsing surface to put the results in; that UI
  would have to be invented too.

**Decision:** reserve, don't build. `spotify` joins `jellyfin` as a reserved
`MediaBackendKind`: config accepts it, `thegn doctor` prints
`media: spotify — reserved`, and `docs/help/media.md` documents the working
path today (spotifyd + MPRIS on Linux; the desktop client everywhere). If the
provider is ever built, this change pre-commits its custody rules: OAuth
tokens live behind SecretRef (`env:VAR` / `file:PATH` — never raw in config),
refresh happens off-loop, and new library/search operations enter
`thegn_core::capability::CATALOG` as rows in that change.

## Security

- **No new credentials.** The reserved spotify kind stores nothing; the
  SecretRef custody rule above is pre-committed for the future provider.
- **Process surface:** viz spawns one user-installed binary (`cava`) with a
  thegn-generated config file in the state dir; it reads system audio via the
  user's session audio server — no elevation, no network. The spawn uses the
  standard argv path (no shell interpolation of config values).
- **Blast radius:** nothing here adds a write surface, an external door, or a
  catalog row. Sandbox: cava runs on the host session (audio servers are not
  reachable from the pane sandbox anyway); it is chrome-adjacent, not a pane,
  so `[sandbox.limits]` wrapping is not applied — its cost is bounded by the
  popup-open lifecycle instead.

## Open questions

- Should the docked popup also be openable from the System ▸ Media panel
  section's `↵` (today it opens the same overlay — keep, just re-anchored)?
  Assumed yes: one popup, one anchor rule.
- cava on Windows is not packaged meaningfully; `auto` resolves to none there.
  Acceptable — viz is Linux/macOS charm until `native` exists.
- Whether `players_priority` should gain a `spotify`-first default when the
  reserved kind is selected — deferred; reserved kinds should be inert.
