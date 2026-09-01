# THE-42 architecture design: dock-attached media

Status: ready for implementation in two serial chunks. This is an architecture
and execution plan, not an implementation or an e2e re-record.

## Decision summary

THE-42 is primarily a surface correction. The branch already has a normalized
media model, an object-safe boxed backend, a statusbar badge, a `System ▸ Media`
section, asynchronous refresh, MPRIS/MPD/mpv/SMTC/macOS implementations, and a
doctor probe. The missing behavior is that the useful surface is still a
centered modal. Make `Section::Media` the canonical dock-attached list/detail
surface and make `Alt-m`, `media-open-panel`, and the badge click focus/open
that section. Keep the existing media action IDs so the keymap, palette,
completion, and control catalog do not grow.

Spotify is a reserved backend kind, not an OAuth client. Existing MPRIS,
SMTC, and AppleScript paths already control the Spotify desktop client or
spotifyd. `spotify_player` and `spotatui` are interactive player UIs, not a
stable one-file machine-control protocol in this tree; a subprocess parser
would be vendor coupling without a defined contract. Do not add Spotify Web
API, OAuth, credentials, search, or library operations in THE-42.

The optional visualization is rejected for this change. The current providers
do not expose level samples. `cava` would capture system audio rather than
sample a provider and would add a process/reader/wake lifecycle; a fake meter
from position is misleading. Do not add `[media.viz]`, a capture thread, a
visualizer dependency, or e2e freeze machinery. A future provider that exposes
levels may add a caps bit and boxed optional operation, but only with a proof
that the operation is absent at idle and is stopped while paused.

## Verified branch audit and draft disposition

| Claim or ask                  | Evidence on this branch                                                                                                                                                                                                                | Decision                                                                                                                                                                                                                                            |
| ----------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Existing now-playing surface  | `crates/thegn-core/src/media.rs:1-11` re-exports the pure leaf model; `crates/thegn-media/src/model.rs:184-257` defines `MediaState`, `now_playing`, `badge`, and position formatting.                                                 | Already satisfied as a model; add a core-owned rendering policy without introducing host types.                                                                                                                                                     |
| Existing panel section        | `crates/thegn-host/src/panel/mod.rs:159-161` declares `Section::Media`; `crates/thegn-host/src/panel/sections/mod.rs:817-849` dispatches its body; `crates/thegn-host/src/panel/sections/media.rs:12-128` renders the current summary. | Already satisfied as a summary. Expand it to list/detail and make it canonical.                                                                                                                                                                     |
| Existing full control surface | `crates/thegn-host/src/media_overlay.rs:1-6,28-68` owns the queue, art, selection, and transport operations; `:200-218` uses `Anchor::Center`.                                                                                         | Replace the modal owner with panel state/rendering. Do not re-anchor the modal; that would preserve two competing surfaces.                                                                                                                         |
| Existing async refresh        | `crates/thegn-host/src/media_watch.rs:134-209` resolves and watches/polls off-loop; `:211-273` drains snapshots and coalesces repaint.                                                                                                 | Reuse the watcher/channel/waker path; remove its overlay coupling. No UI-loop D-Bus, subprocess, or timer.                                                                                                                                          |
| Existing provider seam        | `crates/thegn-media/src/lib.rs:98-121` has `MediaCaps`; `:123-216` has the boxed `MediaBackend` and optional defaults; `:269-291` resolves boxed clients.                                                                              | Retain `Box<dyn MediaBackend>`, make the requested minimum operation/capability relationship explicit, and keep vendor calls in implementation files. No delegation enum.                                                                           |
| Existing implementations      | `crates/thegn-media/src/lib.rs:31-55` lists MPRIS, playerctl, MPD, mpv, SMTC, and macOS modules.                                                                                                                                       | Keep MPD (already covers mpd/mpc/rmpc) and existing cross-platform control. Move Linux D-Bus code under `thegn-media/src/platform/linux/` as required by the platform rule.                                                                         |
| Existing doctor seam          | `crates/thegn-svc/src/seam/registry.rs:441-490` reports media selection, reserved kinds, and OS availability; `:641-665` tests reserved reporting.                                                                                     | Add the reserved Spotify row and keep probes cheap/local. No network or player connection from doctor.                                                                                                                                              |
| Existing config/docs          | `crates/thegn-core/src/config.rs:1435-1528`, `config_media.rs:43-69`, and `config/config.toml.example:4156-4193` define/document `[media]`; `test/env-overlay-ratchet.txt:152-161` pins the existing shallow keys.                     | Add only `spotify` as a reserved enum value and update wording. Do not add viz keys; therefore no new env-overlay entries. Preserve `overlay_on_badge_click` as a compatibility key whose documented meaning becomes “open the docked media panel”. |
| Draft’s docked popup          | Draft proposal `openspec/changes/expand-media-surfaces/proposal.md:27-34` and design `:14-30` propose an anchored overlay.                                                                                                             | Prune: the lead framing requires a dock-attached panel section, not a second popup. `Anchor::At` is not needed.                                                                                                                                     |
| Draft’s cava visualizer       | Draft proposal `:35-45`, design `:47-77`, and spec `openspec/changes/expand-media-surfaces/specs/media/spec.md:30-71` choose external `cava`.                                                                                          | Prune: it is not provider-exposed levels. The pause/idle and provider-source constraints make this out of scope.                                                                                                                                    |
| Draft’s Spotify posture       | Draft proposal `:46-53`, design `:79-111`, and spec `:73-96` reserve Spotify and reject OAuth.                                                                                                                                         | Retain, with the one-file CLI test clarified: no stable adapter is present, so Spotify remains reserved and current MPRIS support is documented.                                                                                                    |
| Draft’s e2e instructions      | Draft proposal `:87-88` requests re-recording.                                                                                                                                                                                         | Do not re-record in this architecture pass. List likely panel snapshots for the coder/reviewer to inspect.                                                                                                                                          |

## Invariants and non-goals

The implementation must preserve the hard rules in `CLAUDE.md:40-58` and
`docs/ARCHITECTURE.md:54-84,101-149,199-243`:

- Idle input remains a blocking `poll_input(None)`. Media work is a channel
  producer plus `TerminalWaker`; all D-Bus, MPD, CLI, art, and queue work stays
  off-loop. A paused/stopped player adds no visualizer, timer, process, or wake
  source. Existing fallback polling remains the provider’s explicit degraded
  path, not a new panel timer.
- The panel remains the source of truth for painted rows and hit targets. Do
  not infer mouse coordinates in `run.rs` from a second layout calculation.
  Render degradation uses the existing glyph/color chokepoints; media rows may
  not add literal Unicode glyphs at draw sites.
- `thegn-core` stays substrate-free. The branch intentionally keeps the
  normalized DTO in the C-dependency-free `thegn-media` leaf and exposes it
  through `thegn_core::media`; `thegn-core/src/media.rs:4-9` records why. A
  literal move of the DTO into core would make the per-OS leaf depend on core
  or duplicate the model, violating the cross-target boundary. Put the new
  pure `MediaRenderPolicy`/panel projection and its tests in the core-facing
  media module, while retaining the leaf-owned DTO as the one canonical model.
- No new external door or capability-catalog row. Panel mouse targets dispatch
  existing internal media actions; no API/MCP/gRPC/CLI verb is added. The
  control schema snapshot and completion-slot ratchet must remain unchanged.
- No SQLite migration and no live-state invocation. If a coder runs a
  `thegn` command, it must set `XDG_STATE_HOME` to a fresh temporary directory.

## Target design

### 1. Canonical right-panel surface

`Section::Media` stays in the System tab and remains hidden when media is
disabled, following `panel::resolve_order` and the existing `PanelData.media`
contract (`crates/thegn-host/src/panel/mod.rs:559-591`). The expanded section
uses the existing `PanelUi` width tiers and frame builder:

- Normal: current track/status summary and a compact transport row.
- Half: a selectable player/source list followed by the selected source’s
  now-playing detail and transport controls.
- Full: source list plus now-playing detail and the provider queue, with
  capability-gated controls. Art remains an optional detail decoration and is
  dropped first at narrow widths, never required for control.

The selected row and queue live in a small `MediaPanelState` sibling module,
not in `run.rs` or a new global overlay. Snapshot, queue, and art deliveries
are accepted only if their player/track identity still matches. A provider
failure clears/degrades the affected detail and leaves the section usable.

Open behavior is uniform:

- `Alt-m`/`Action::MediaOpenPanel` calls the existing panel-section opener for
  `Section::Media` and focuses the panel. It never allocates a modal.
- A media badge click focuses/opens the same section when
  `overlay_on_badge_click` is true. The legacy field is retained for config
  compatibility and documented with the new panel meaning.
- `Enter` on the open Media section selects/activates the highlighted row;
  it does not open another surface. When media is disabled, the existing
  status message is retained.
- `Esc`, tab changes, width cycling, and section navigation remain generic
  panel behavior. Do not copy the sandbox’s `s/r/l` container lifecycle
  semantics onto media: `docs/help/sandboxing.md:192-205` and
  `docs/help/panel.md:289-291` reserve those letters for stop/restart/logs.
  Media keeps its existing `s` shuffle and `L` loop bindings, and uses the
  existing media action IDs for all other transport operations.

### 2. Keyboard and mouse hit table

The row-producing code in `panel/sections/media.rs` attaches targets to the
same `PanelFrame` consumed by rendering and click resolution. Extend the
panel hit enum only for media controls; do not make the generic `Row` index
carry vendor methods.

| Painted target                 | Keyboard                                                    | Mouse                                                          | Result                                                                          |
| ------------------------------ | ----------------------------------------------------------- | -------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Media section header           | `Shift-J/K`, number jump, generic panel navigation          | `PanelHit::OpenSection(Media)`                                 | Open/focus Media.                                                               |
| Source/player row              | `j/k`, `Enter`                                              | `PanelHit::Row(Media, source_index)`                           | Select source; `Enter` activates it through the existing player-selection path. |
| Queue row                      | `j/k`, `Enter`                                              | `PanelHit::Row(Media, queue_index)`                            | Select; `Enter` dispatches the existing queue-item operation off-loop.          |
| Play/pause, next, previous     | Existing media action chords; section `space/n/p`           | `PanelHit::MediaAction(PlayPause/Next/Previous)`               | Same `MediaOp` dispatch and capability/error handling.                          |
| Shuffle/repeat                 | `s`/`L`, existing `media-shuffle-toggle`/`media-loop-cycle` | `PanelHit::MediaAction(Shuffle/Loop)`                          | Same operations; hidden/disabled when caps do not expose them.                  |
| Volume/seek/chapter/fullscreen | Existing `media-*` actions                                  | `PanelHit::MediaAction(...)` only where the control is painted | Same action ID and off-loop operation; no mouse-only behavior.                  |

The generic mouse path in `crates/thegn-host/src/run.rs:13527-13679` already
resolves against the painted frame and focuses a clicked section/row. The new
media-action branch must call a handler that is shared by keyboard and mouse;
it must not run a provider future inline. Add panel hit tests for every row
kind and a click→same-operation assertion.

### 3. Provider seam and implementations

Retain the object-safe `thegn_media::MediaBackend` shape (`lib.rs:123-216`)
and boxed resolution (`lib.rs:269-291`), but ratchet the contract around the
requested capability set:

- `snapshot` is the now-playing read operation; play/pause, next, and previous
  are the baseline transport operations. Volume and queue are optional and
  must have a corresponding caps bit and an `Unsupported`-classified default.
  Existing playlist/seek/chapter/fullscreen extensions follow the same rule.
- Keep dispatch as `Box<dyn MediaBackend>` and put vendor argv, D-Bus object
  paths, MPD protocol details, and parsing in one implementation file each.
  Do not add a per-method delegation enum or let the panel know provider kind.
- Keep the leaf’s no-internal-dependency boundary. The leaf cannot implement
  `thegn_core::seam::SeamError` without creating a dependency cycle; preserve
  its local `MediaError` and document the mapping at the host/service edge,
  while using the shared caps/boxed/probe vocabulary wherever the owning crate
  permits. Do not “solve” this by making the leaf depend on core.
- Move native Linux D-Bus code from the top-level `mpris.rs` into
  `crates/thegn-media/src/platform/linux/mpris.rs`, with module wiring in
  `platform/mod.rs` and `platform/linux/mod.rs`. `playerctl` may share that
  Linux implementation directory but remains its own file. This satisfies the
  platform rule without changing runtime fallback order.
- Native MPD already covers the issue’s `mpc`/rmpc reference: the current
  config docs and `crates/thegn-media/src/lib.rs:241-244` say so. Do not add an
  `mpc` subprocess adapter.
- Add `MediaBackendKind::Spotify`/leaf `BackendKind::Spotify` as reserved and
  make resolution inert. `thegn doctor` reports it with
  `ProbeReport::reserved`; it reads no credentials and performs no network
  work. Existing MPRIS/SMTC/AppleScript Spotify support is the documented
  working path.
- Keep `thegn doctor` probes deterministic and cheap. `Auto` reports runtime
  resolution; reserved Spotify reports reserved; native provider availability
  may report OS/binary presence but must not connect to a player or block.

### 4. Pure model and rendering policy

The existing `MediaState` is already pure and unit-tested in
`crates/thegn-media/src/model.rs:305-422`, and is publicly surfaced at
`thegn_core::media`. Add the new panel projection/render policy beside the
re-export in `crates/thegn-core/src/media.rs`: it should decide rows, labels,
capability visibility, width-tier reduction, and paused/stopped animation
policy from plain data only. It must not import termwiz, tokio, D-Bus, a
terminal surface, or a provider implementation. Test the policy with fixed
snapshots/data cases in `thegn-core`.

Do not synthesize a VU/spectrum from position or metadata. If a future backend
adds provider-level samples, it must add `MediaCaps::levels` plus a boxed
optional levels operation, and the host must subscribe only while the Media
section is visible and the provider is playing. A paused provider must stop
sampling and have zero added wake cost.

## Configuration, docs, and ratchets

The only new config value is the reserved enum token `spotify`. Update the
schema enum, lowering match, config enum coverage/strict-reserved tests, and
the example’s backend list. Keep and reinterpret
`overlay_on_badge_click`; do not introduce `panel_on_badge_click` or any
`[media.viz]` table. Because no new shallow field is added, keep the existing
`media.*` entries in `test/env-overlay-ratchet.txt` unchanged.

Update `docs/help/media.md` and the Media entry/details in
`docs/help/panel.md`: canonical docked panel, list/detail behavior, hit-table
semantics, existing key IDs, Spotify’s supported MPRIS route, and explicit
“visualization rejected for now” rationale. `panel:media` is already claimed by
the media help page, so the help-context/prose ratchets should remain empty;
if a test exposes a new action claim, fix the page and ratchet in the same
chunk. No action IDs are added, so the completion-slot ratchet and
`docs/api/control-v1.json` control-schema snapshot remain unchanged and must
be verified byte-for-byte.

Likely affected snapshot files to review after implementation, but do not
re-record in this work:

- `test/muse/snapshots/panel_system__system/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__160x40__linux.txt`
- `test/muse/snapshots/panel_work__work/xterm__100x30__linux.txt`

The implementation coder must not run e2e or update these snapshots as part of
the two scoped commits.

## Sequencing and ownership

Chunk 1 is the serial predecessor: it establishes the reserved kind, provider
module boundary, core policy contract, and doctor/config tests. Chunk 2 then
replaces the host overlay path with the panel surface and updates help. The
chunks touch disjoint files; the dependency is type/API dependency only, not
file overlap. The final architect commit contains this design and both chunk
specifications only.
