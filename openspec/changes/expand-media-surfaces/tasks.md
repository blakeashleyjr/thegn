# Tasks — media surface expansion

## 1. Docked Now-Playing popup

- [ ] 1.1 Statusbar exposes the media badge's hit rect to the overlay opener
      (same pattern the calendar popup uses for the date/clock widgets).
- [ ] 1.2 `media_overlay.rs`: open with `Anchor::At` computed adjacent to the
      badge rect, opening upward from the statusbar; clamp via the existing
      `layer.rs` clamp; corner fallback when no badge rect exists this frame.
- [ ] 1.3 Small-terminal degradation unchanged (art drops first) — verify the
      existing narrow-popup tests still pass re-anchored.
- [ ] 1.4 `docs/help/media.md`: describe the docked anchor (help-prose
      ratchet — mention by label, keep `media-panel` claims intact).
- [ ] 1.5 Re-record affected e2e baselines (`just e2e-update`, review diffs).

## 2. `[media.viz]` config + seam

- [ ] 2.1 `config_media.rs`: `[media.viz]` — `enabled` (false), `backend`
      (`auto` | `cava` | `native` | `none`), `fps` (15, clamped 5..=30),
      `bars` (auto-fit default); `config_enum!` for the kind; validation
      warnings for out-of-range fps. Document every key in
      `config/config.toml.example`.
- [ ] 2.2 `seam/registry.rs`: viz probe — implemented (`cava` on PATH),
      unavailable (with reason), reserved (`native`); unit tests beside the
      existing `media_probes` tests.
- [ ] 2.3 Pure frame model in core: parse cava raw ASCII frames → normalized
      bar levels; clamp/decimate to the popup width; unit tests to the
      coverage gate (pure logic, no subprocess).

## 3. cava lifecycle (host)

- [ ] 3.1 `media_viz.rs`: generate a cava config in the state dir (raw output,
      bar count, framerate), spawn on popup-open-while-playing, kill on close
      or playback-stop debounce; reader thread → channel → waker pulse,
      coalesced to the fps cap; `Background` QoS class on the thread.
- [ ] 3.2 Failure paths: binary missing, process crash, malformed frames — all
      degrade to strip-absent; no status noise; doctor carries the reason.
- [ ] 3.3 Render the strip in `media_overlay.rs` (hues via theme, glyphs via
      the caps chokepoint — no literals at draw sites; ratchet-clean).
- [ ] 3.4 `e2e_freeze.rs`: pin viz frames to a fixed synthetic pattern under
      `THEGN_E2E=1`.
- [ ] 3.5 Render-plan check: viz frames while the popup is open take the
      existing overlay damage path; no new wake source while the popup is
      closed (idle-guard stays green).

## 4. Spotify reserved kind + docs

- [ ] 4.1 `MediaBackendKind::Spotify` (reserved): config accepts, backend
      resolution treats as reserved-inert, `media_probes` reports reserved
      (extend the pinned-kind tests deliberately).
- [ ] 4.2 `docs/help/media.md`: spotifyd/MPRIS recipe + a note on what the
      reserved Web-API provider would add and why it isn't built.

## 5. Wrap-up

- [ ] 5.1 Unit tests: anchor fallback, frame parsing, probe matrix, reserved
      kinds (core logic to the 95% gate; subprocess seam smoke-covered).
- [ ] 5.2 Run `just ci` once (includes openspec validate); re-recorded e2e via
      `just ci-local` where frames changed.
