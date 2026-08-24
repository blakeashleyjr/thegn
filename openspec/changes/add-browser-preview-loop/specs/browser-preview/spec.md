# Browser Preview

## ADDED Requirements

### Requirement: No embedded browser engine

thegn SHALL NOT link or embed a browser engine in any of its processes. Web
preview surfaces MUST be one of: the user's external browser (the existing
open ladder), a configured **terminal-browser program running as an ordinary
PTY pane**, or a **static snapshot image** produced by an external provider.
An interactive non-PTY web view is out of scope for this capability
permanently.

#### Scenario: Preview never grows an engine dependency

- **WHEN** any browser-preview surface is exercised (pane, snapshot, or open)
- **THEN** the only web-rendering processes involved are external programs the
  user configured, spawned and contained like any other tool

### Requirement: Browser pane runs a configured terminal browser as a normal pane

WHEN `[browser] pane_command` is non-empty, thegn SHALL provide a
`browser-pane` action that renders the template (`{url}`, optional
`{profile_dir}`; `THEGN_BROWSER_PROFILE_DIR` exported) against the active
preview URL and spawns it as an ordinary center pane — PTY-backed,
sandbox/cgroup-contained at spawn, daemon-owned, session-persisted like any
pane. WHEN `pane_command` is empty (the default), the pane surface SHALL be
hidden entirely (no palette entry, no keybind effect). The pane's browser
profile/state directory SHALL live under thegn's XDG state directory, scoped
by `[browser] profile_dir_mode` (`per-workspace` default, `per-profile`,
`shared`), and SHALL NOT be a path into any installed browser's profile.

#### Scenario: Opening the pane on a detected forward

- **WHEN** `pane_command = "carbonyl {url}"` is set and the user invokes
  `browser-pane` while a forward for container port 3000 is active
- **THEN** a center pane spawns running the rendered command against the
  forward's host URL, contained like any other pane

#### Scenario: Unconfigured surface stays invisible

- **WHEN** `pane_command` is empty
- **THEN** `browser-pane` does not appear in the command palette and the
  feature adds no cost

#### Scenario: Per-workspace profile isolation

- **WHEN** `profile_dir_mode = "per-workspace"` and panes are opened from two
  different workspaces
- **THEN** each pane receives a distinct profile directory under thegn's XDG
  state, so a login made in one workspace's pane is absent from the other's

### Requirement: Installed browsers' data is never imported

thegn MUST NOT read, copy, or decrypt any installed browser's cookie, history,
session, or credential store, and MUST NOT request OS-keychain material
belonging to another application. Authenticated preview flows SHALL be served
by (a) the user's own browser via the existing open action, or (b) the pane
browser's own persistent profile that the user logs into directly.

#### Scenario: Pane starts unauthenticated by design

- **WHEN** a browser pane is opened for the first time in a workspace
- **THEN** its profile directory starts empty; any authentication happens by
  the user logging in inside the pane, and no path under an installed
  browser's profile directory is accessed

### Requirement: URL snapshot provider seam

The system SHALL expose one-shot URL rasterization as a provider seam:
`[browser.snapshot] kind = "none" | "servo-fetch" | "chromium" | "custom"`
(implemented-or-`reserved`), with `custom` taking a `command` template that
MUST contain `{url}` and `{out}` placeholders. Command planning (template
render, quoting) SHALL be pure `thegn-core` logic; execution SHALL run
off-loop under the shared background containment wrap with a
`timeout_ms` watchdog, deliver the decoded raster over a channel, and pulse
the terminal waker. The rendered snapshot SHALL draw through the existing
`preview_gfx` kitty graphics chokepoint, degrading to a text placeholder on
non-kitty terminals. `thegn doctor` SHALL probe the configured kind (binary
resolution and honest `reserved`/not-configured verdicts) without fetching any
URL. Snapshot fetches MUST carry no cookies or ambient user identity.

#### Scenario: Configured provider renders the Forward panel snapshot

- **WHEN** `kind = "servo-fetch"` is configured and the user invokes
  `browser-snapshot` on an active forward in a kitty-capable terminal
- **THEN** the shot runs off-loop, the panel shows the resulting image via the
  graphics overlay, and the loop never blocks on the fetch

#### Scenario: Default is off

- **WHEN** `kind = "none"` (the default)
- **THEN** no snapshot surface is offered and no provider binary is probed as
  missing

#### Scenario: Invalid custom template is rejected

- **WHEN** `kind = "custom"` with a `command` missing `{out}`
- **THEN** config validation reports the template error and the surface stays
  hidden

### Requirement: Snapshot auto-refresh is debounced and event-driven

WHEN `[browser.snapshot] auto = true`, the system SHALL re-shoot the active
preview URL when the forward detector reports a change, debounced so a
flapping dev server produces bounded shots, running entirely off the event
loop. The idle loop MUST NOT gain any poll for this feature.

#### Scenario: Dev server restart refreshes the picture once

- **WHEN** a forwarded dev server restarts and rebinds within the debounce
  window
- **THEN** at most one new snapshot is shot after the window closes, and an
  idle instance with no forward activity performs no snapshot work

### Requirement: `browser.drive` drives the preview surface

The existing `browser.drive` catalog row SHALL be implemented on the HTTP,
gRPC, and CLI surfaces against the preview surface, keeping the stable
`BrowserCommand` wire contract: `navigate` retargets the active preview URL
(snapshot re-shoots; a pane is driven where its tool supports it, else
respawned on the new URL), `reload` re-shoots or reloads/respawns, and `back`
applies where the pane tool supports it and is otherwise a no-op reported in
the status line. Delivery SHALL use the DB `intents` mailbox
(`browser_drive` kind, claim-and-delete by the compositor's model refresh),
matching the `worktrees.open` precedent, and an accepted call means
_enqueued_. WHEN no preview surface is configured, the daemon MUST return a
"browser preview not configured" precondition error rather than
`Unimplemented`. The verb stays gated by `required_scope` from the capability
catalog; no new catalog rows are added, and the MCP/plugin projections remain
owned by the in-flight MCP write-tools work and THE-39's generic plugin
dispatch respectively.

#### Scenario: Navigate over the control API

- **WHEN** a paired client with the required scope calls `browser.drive` with
  `navigate` to an active forward's URL while a snapshot provider is
  configured
- **THEN** the call is acknowledged, an intent is enqueued, and the compositor
  retargets and re-shoots within approximately one model-refresh tick

#### Scenario: Honest error when unconfigured

- **WHEN** `browser.drive` is called while both `pane_command` and
  `[browser.snapshot] kind` are unset
- **THEN** the caller receives a "browser preview not configured" error, not
  a 501/`Unimplemented`

### Requirement: Drive navigation targets are confined by default

`navigate` targets SHALL be validated against the workspace's active forwards
and loopback origins by default; a target outside that set MUST be rejected
before an intent is enqueued unless `[browser] allow_external_urls = true`.
The confinement check SHALL be pure core logic.

#### Scenario: External URL rejected by default

- **WHEN** `browser.drive` `navigate` names `https://example.com` with
  `allow_external_urls` unset
- **THEN** the call fails with a confinement error and no intent is enqueued

#### Scenario: Operator opt-out

- **WHEN** `allow_external_urls = true` is configured
- **THEN** the same call is accepted and delivered
