# Media

## ADDED Requirements

### Requirement: The Now-Playing popup docks to the media badge

The Now-Playing popup SHALL open anchored adjacent to the statusbar media
badge — following the same dock-to-owner convention as the calendar popup —
when opened by clicking the badge or by the `media-panel` chord, opening
upward from the statusbar. When the badge has no on-screen rect (nothing
active, the widget removed from `[bars]`, or the statusbar hidden), the popup
MUST still open, anchored to the statusbar-adjacent corner — the chord never
goes dead. Interior behavior (cover art, scrubber, transport, queue) is
unchanged, and the popup MUST degrade on small terminals exactly as the
centered modal did (art drops first).

#### Scenario: Opening from the badge

- **WHEN** the user clicks the statusbar media badge (or presses `Alt-m`)
  while the badge is rendered
- **THEN** the Now-Playing popup opens anchored adjacent to the badge, not
  centered

#### Scenario: Opening with no badge on screen

- **WHEN** the user presses `Alt-m` while no media badge is rendered
- **THEN** the popup opens anchored near the statusbar edge and is fully
  functional

### Requirement: Optional audio visualization in the Now-Playing popup

An audio-visualization strip SHALL be available inside the Now-Playing popup,
governed by `[media.viz]` with `enabled = false` by default. The visualizer
backend SHALL be a provider seam kind (`auto` | `cava` | `native` | `none`)
where `cava` is implemented by driving the user-installed `cava` binary in raw
output mode and `native` is reserved; `thegn doctor` MUST probe the selected
kind and report implemented/unavailable/reserved.

The visualizer MUST cost nothing when off: with `enabled = false`, or while
the popup is closed, no process runs, no thread exists, and no wake source is
registered. While active, the capture process is spawned on popup open (with
playback active) and terminated on popup close; frames arrive over a channel
with a waker pulse, coalesced to the configured `fps` cap; each frame damages
only the overlay. A missing or failing backend binary MUST degrade to the
strip being silently absent, with the reason visible only in `thegn doctor`.
Under `THEGN_E2E=1` the strip MUST render a pinned synthetic pattern.

#### Scenario: Disabled by default

- **WHEN** `[media.viz]` is unconfigured and the Now-Playing popup is opened
- **THEN** no visualizer process is spawned and the popup renders without a
  viz strip

#### Scenario: Enabled with cava installed

- **WHEN** `enabled = true`, the resolved kind is `cava`, playback is active,
  and the popup opens
- **THEN** cava is spawned, spectrum bars render in the popup at no more than
  the configured fps, and closing the popup terminates the process

#### Scenario: Enabled with no backend available

- **WHEN** `enabled = true` but the `cava` binary is not on PATH
- **THEN** the popup opens without the strip, nothing errors, and
  `thegn doctor` reports the viz backend unavailable with the reason

#### Scenario: Reserved native kind

- **WHEN** `backend = "native"` is configured
- **THEN** config loads, the strip is absent, and `thegn doctor` reports the
  kind as reserved

### Requirement: Spotify is a reserved media backend kind

`spotify` SHALL be accepted as a `[media] backend` kind and reported as
`reserved` by `thegn doctor`, reserved for a future Spotify Web-API provider
(library/playlist search, Spotify Connect device transfer). Until that
provider exists, selecting it MUST be inert (no network, no credential reads),
and Spotify control SHALL be documented as working today through the existing
backends (MPRIS — desktop client or spotifyd — on Linux, SMTC on Windows,
AppleScript on macOS). When implemented, the provider MUST keep OAuth tokens
behind SecretRef (`env:` / `file:` — never raw in config) and MUST add its
externally invokable operations as capability-catalog rows in that change.

#### Scenario: Reserved kind is inert and diagnosed

- **WHEN** `[media] backend = "spotify"` is configured
- **THEN** config loads, no backend activates, no network or credential access
  occurs, and `thegn doctor` prints the media seam as reserved for that kind

#### Scenario: Spotify controlled through MPRIS today

- **WHEN** `backend = "auto"` on Linux and spotifyd (or the Spotify desktop
  client) is running
- **THEN** the badge, panel section, popup, and transport binds control it
  with no Spotify-specific configuration
