# THE-42 architect review — revision 1

## Gap

The docked panel does not receive the resolved backend's actual
`thegn_media::MediaCaps` or player enumeration:

- `crates/thegn-host/src/panel/media.rs::caps` reconstructs capabilities from
  `MediaState`. This cannot represent provider capabilities. In particular,
  `MediaKind::Video` makes the panel paint chapter controls even though native
  MPRIS and playerctl declare `chapters: false`; it also conflates video
  classification with fullscreen support.
- `MediaPanelState::sources` is populated only by pushing the player named by
  each now-playing snapshot. The panel never consumes `MediaBackend::players()`,
  so the promised Half/Full source list is normally a one-item history rather
  than the available player/source list.
- Queue delivery is an untagged `Vec<QueueItem>`. The review correction now
  refreshes it when snapshot identity changes, but a response still needs an
  explicit request identity/generation so an older request cannot be accepted
  merely because the current player/track later has the same value.

## Required correction

1. Carry the actual resolved `MediaCaps` alongside every now-playing snapshot
   (including operation results), or use an equivalent identity-tagged host
   snapshot DTO. Preserve `None` and disabled behavior. The panel must feed
   those caps to `MediaRenderPolicy`; do not infer chapters/fullscreen or other
   optional operations from `MediaKind` or incidental snapshot fields. A video
   provider with `chapters: false` must not paint chapter targets, and a
   provider with `fullscreen: false` must not paint fullscreen.
2. Add an off-loop player/source-list request using the existing
   `MediaBackend::players()` operation and deliver it through the panel state.
   Keep provider work out of the event loop, retain the current source
   selection/restart behavior, and identity/generation-check late deliveries.
   The Half/Full source rows must represent the delivered list, with the active
   source highlighted and no duplicate names.
3. Tag queue requests and deliveries with the request's player/track identity
   and/or generation. Retain the refresh-on-new-snapshot behavior from the
   review fix, and reject late responses from an older request even when the
   currently displayed snapshot happens to compare equal again.

Add focused pure/host tests for provider-capability gating (including the
MPRIS video case), source-list delivery/selection and duplicate elimination,
and stale queue/source responses. Keep the existing action catalog/control
schema unchanged; no overlay, visualizer, provider OAuth/network client, or
new external door is part of this correction.

Suggested scope: `crates/thegn-host/src/panel/media.rs`,
`crates/thegn-host/src/panel/sections/media.rs`,
`crates/thegn-host/src/media_ctl.rs`, `crates/thegn-host/src/media_watch.rs`,
`crates/thegn-host/src/run.rs`, and the minimal `thegn-media` model/aggregate
surface needed to carry caps without introducing a dependency cycle.
