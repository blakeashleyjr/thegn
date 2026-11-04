## ADDED Requirements

### Requirement: Resident plugins can contribute application tabs

The host SHALL turn each accepted `AppTab` contribution with a declared surface
into an app tab whose id is `plugin:<plugin>:<contribution>`. Valid `update`
messages carrying an `app` AppView SHALL replace that tab's view; updates for an
undeclared surface, from another plugin, with a non-increasing revision, or
failing validation SHALL be rejected. Removing the plugin SHALL remove its tab
and select Work if it was active; a config reload SHALL keep the tab (and its
local drafts) until the new discovery result replaces it.

#### Scenario: A plugin contributes a workspace

- **WHEN** a resident plugin declares an accepted `AppTab` contribution and
  sends a valid app update
- **THEN** its namespaced id participates in `[apps]` ordering and startup
  selection
- **AND** the view renders through the app-tile bridge
- **AND** another plugin cannot update that surface

### Requirement: App transport preserves bounded causal delivery

One resident session SHALL allocate host request ids from a single allocator
shared by app events and `provider.call`. Host-to-plugin admission SHALL bound
queued frames and bytes including the frame being written, reserving room for
replies and lifecycle notifications. Replaceable notifications (form change,
selection, resize) MAY coalesce only after the latest reliable frame.
Plugin-to-host coalescing of `update` frames MUST NOT cross a request or a
focus-bearing update, and per-plugin queued bytes SHALL be bounded.

#### Scenario: A noisy plugin floods replaceable updates

- **WHEN** one plugin floods app updates
- **THEN** memory and per-drain work stay bounded and a pending focus request is
  not lost

### Requirement: Host ownership remains available inside plugin app tabs

While a plugin app tab is focused, host overlays SHALL own input first; the
user's configured Quit, Detach, QuitKill, OpenPalette and Help chords and the
app-tab switch chords SHALL pre-empt app input, including modified multi-key
sequences; and the `plugin-app-drafts` / `plugin-app-requests` actions SHALL
open the draft and request recovery inspectors. No app key is hard-coded outside
the keymap. Built-in tiles keep their existing input ownership.

#### Scenario: A user rebinds quit

- **WHEN** the user binds Quit to `Ctrl x Ctrl c` and focuses a plugin app
- **THEN** that sequence quits, and `Ctrl q` is delivered to the app

### Requirement: Plugin focus requests are causal

A plugin MAY ask to be focused only by echoing a focus ticket the host issued
for a palette invocation. The ticket SHALL be single-use, expire after 5
seconds, and match the plugin's live session generation, the surface and the
navigation epoch; it SHALL be refused while a host modal owns input, and a
refused ticket SHALL never be deferred.

#### Scenario: Focus after navigation

- **WHEN** the user switched tabs or typed after invoking the palette action
- **THEN** the plugin's focus request is refused

### Requirement: Idle plugin apps add no wake sources

An app tab with no pending reliable request SHALL contribute no poll timeout and
no waker pulse; the host SHALL arm a timeout only for the earliest pending
request deadline. Hidden apps SHALL be told `visible: false` with their effective
background permission.

#### Scenario: An idle app tab

- **WHEN** a plugin app is visible with a rendered view and nothing pending
- **THEN** the loop's poll timeout is unchanged (`None` when otherwise idle)

### Requirement: Plugin app tabs are experimental

App tabs SHALL be governed by the `plugin_apps` release-channel feature, which
is dev-only. In the stable channel, `AppTab` contributions SHALL be rejected at
load with a diagnostic naming `THEGN_CHANNEL=dev`, and `plugin:` ids in
`[apps]` SHALL be neutralised at config load.

#### Scenario: Stable channel

- **WHEN** thegn runs in the stable channel with a plugin declaring an AppTab
  and `[apps] default_tab = "plugin:p:app"`
- **THEN** no plugin tab appears, the default tab is `work`, and `thegn doctor`
  lists `plugin_apps` as gated
