# THE-42 chunk 1 — provider seam, pure policy, reserved Spotify

## Ownership and sequencing

This chunk is the serial predecessor to chunk 2. It touches no host panel or
help files; chunk 2 may begin after this commit. No file overlaps chunk 2.

## Files to touch (exact paths)

Implementation files:

- `crates/thegn-core/src/media.rs`
- `crates/thegn-core/src/config.rs`
- `crates/thegn-core/src/config_media.rs`
- `crates/thegn-core/src/config_tests_coverage.rs`
- `crates/thegn-core/src/config_validate.rs`
- `crates/thegn-media/src/model.rs`
- `crates/thegn-media/src/lib.rs`
- `crates/thegn-media/src/platform/mod.rs` (new)
- `crates/thegn-media/src/platform/linux/mod.rs` (new)
- `crates/thegn-media/src/platform/linux/mpris.rs` (moved from the current top-level `mpris.rs`)
- `crates/thegn-media/src/platform/linux/mpris_cli.rs` (moved from the current top-level `mpris_cli.rs`)
- `crates/thegn-media/src/mpris.rs` (remove after the move)
- `crates/thegn-media/src/mpris_cli.rs` (remove after the move)
- `crates/thegn-svc/src/seam/registry.rs`
- `crates/thegn-svc/src/conformance.rs` (only if the reserved-kind matrix needs the new row)
- `config/config.toml.example`
- `test/platform-cfg-media-ratchet.txt`

Ratchet/snapshot verification surfaces, which must remain unchanged because
this chunk adds no new shallow config key, CLI value-taking argument, action,
or control-wire type:

- `test/env-overlay-ratchet.txt`
- `test/completion-slot-ratchet.txt`
- `docs/api/control-v1.json`
- `test/async-trait-ratchet.txt`

If a relevant test requires a deliberate ratchet edit, make that edit in this
commit and explain it in the commit body; do not defer it to chunk 2.

## Approach

1. Keep `MediaState` as the one canonical normalized DTO in the core-free leaf;
   `thegn-core::media` remains its public core-facing path for the documented
   cross-target reason. Add pure core-facing `MediaRenderPolicy`/projection
   functions and unit tests there. The policy takes plain `MediaState`, caps,
   and width inputs and returns data decisions, never a termwiz surface.
2. Preserve `MediaBackend` as `Box<dyn MediaBackend>` and make the requested
   `snapshot`, transport, volume, and queue relationship explicit in docs,
   caps, and defaults. Do not introduce an enum router or a core dependency in
   the media leaf. Keep the existing MPD implementation; it already covers
   `mpd`/`mpc`/rmpc-style servers.
3. Move Linux MPRIS D-Bus code and its Linux CLI fallback into the new
   `thegn-media/src/platform/linux/` module tree. Keep all zbus imports,
   object paths, signal streams, and playerctl argv inside those implementation
   files. Preserve async behavior and the current signal/poll fallback.
4. Add `Spotify` as a reserved value in both config/lowering and leaf backend
   kind. Resolution returns `None`; `thegn doctor` uses
   `ProbeReport::reserved`. Do not read a token, contact Spotify, or invoke
   `spotify_player`/`spotatui`.
5. Update the config example to list Spotify as reserved and describe the
   existing MPRIS/SMTC/AppleScript route. Keep `overlay_on_badge_click` as a
   compatibility key; the host chunk will give it docked-panel semantics.

Do not add `[media.viz]`: no current provider exposes levels, and the rejected
cava design is not provider sampling and would create an unnecessary wake/process
lifecycle. Do not add config env knobs.

## Tests to run

Run only scoped checks; do not run `just test`, `just ci`, a workspace build,
e2e, a migration, or the built binary.

- `just quick thegn-core`
- `cargo nextest run -p thegn-core media`
- `cargo nextest run -p thegn-core config`
- `just quick thegn-media`
- `cargo nextest run -p thegn-media model`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc media_probes`

Also run the focused ratchet tests exposed by the touched crates if their
normal quick command does not include them. Any `thegn` invocation must set
`XDG_STATE_HOME` to a fresh temporary directory; no live state DB is in scope.

## Done criteria

- `MediaRenderPolicy` and its tests are substrate-free; core coverage and crate
  boundary checks remain valid.
- MPRIS D-Bus code exists only under `thegn-media/src/platform/linux/`; no
  D-Bus or vendor CLI call appears in host/core code.
- `MediaBackend` remains object-safe, optional ops are caps-gated/defaulted,
  and existing MPRIS/MPD behavior is preserved.
- `backend = "spotify"` loads as a reserved kind, resolves inertly, and is
  reported by `thegn doctor` without network/credential access.
- The config example documents every changed key/value. Existing env-overlay,
  completion-slot, async-trait, and control-schema ratchets are unchanged or
  deliberately updated here with a reason.
- No `[media.viz]`, cava process, FFT dependency, OAuth flow, or new catalog row
  is present.
- Commit exactly with subject: `feat(the-42): harden media seam and reserve spotify`
