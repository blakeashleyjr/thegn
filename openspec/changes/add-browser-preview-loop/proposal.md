# Browser preview loop — see the dev server without leaving the terminal

Linear: THE-13

## Why

The frontend-dev loop is half built. thegn already **detects** a dev server
bound inside a worktree's sandbox (`[forward]` off-loop `ss` detector, AC 368),
**forwards** it to the host's loopback (userspace exec-bridge proxy with
conflict remap, AC 371), and **opens** it — System ▸ Forward's `o` runs the
`[forward] browser` / `$BROWSER` / OS-opener ladder (AC 369). What is missing
is everything after the tab opens somewhere else:

- **Seeing the page in the workspace.** There is no way to keep the preview
  _inside_ thegn — no browser pane, no rendered glance of what the dev server
  is serving. Superset's answer is an embedded Chromium pane; thegn is a
  terminal and must answer in its own lane.
- **Driving it from outside.** `browser.drive` is already a catalog row on
  every surface with a stable wire contract (`BrowserCommand`:
  navigate/reload/back) — and every implementation answers
  `Unimplemented("drive-browser")`. The THE-39 audit calls this out as the
  depth gap: routed everywhere, does nothing.
- The issue also asks for **browser import** — "cookies, history, and sessions
  from Chrome, Firefox, Arc, and 20+ browsers so browser panes start
  authenticated". This proposal evaluates that and **rejects it as designed**
  (see design.md Security); the linked Superset docs do not actually ship it
  either — their consent story is an opt-in third-party CLI touching the
  user's _live_ Chrome profile, not a cookie-store import.

## What Changes

- **`[browser]` config table** (documented in `config/config.toml.example`):
  - `pane_command` — a command template with a `{url}` placeholder naming a
    **terminal browser** (e.g. `carbonyl {url}`, zenbu terminal-browser,
    `w3m {url}`). Empty (the default) hides the pane surface entirely.
  - `profile_dir_mode = "per-workspace" | "per-profile" | "shared"` — where the
    pane browser's own profile/state directory lives (always under thegn's
    XDG state, never the user's real browser profile).
  - `[browser.snapshot]` — a **provider seam** for one-shot URL rasterization:
    `kind = "none" | "servo-fetch" | "chromium" | "custom"` plus a `command`
    template (`{url}`, `{out}`) for `custom`. Implemented-or-`reserved`,
    `Probe` in `thegn doctor`.
- **Browser pane, not browser engine.** A new action `browser-pane` opens the
  configured `pane_command` on the active preview URL as a normal pane (center
  tab; a `[[drawer.tools]]` occupant once `add-drawer-tool-registry` lands —
  the registry, not this change, owns drawer plumbing). The pane is a PTY like
  any other: sandbox/cgroup containment, daemon survival, zero new rendering
  substrate. thegn links **no** browser engine in-process (a spec requirement,
  not just a preference).
- **URL snapshot route.** When `[browser.snapshot]` is configured, the Forward
  panel can render a **screenshot** of a preview URL through the existing
  off-loop rasterize → `preview_gfx` graphics path (kitty protocol today;
  sixel/iTerm remain AF 399's open tail — non-kitty terminals get a text
  placeholder, degrade-at-the-edges). Glanceable, non-interactive, honest
  about being a picture. Refresh on demand (`browser-reload`), and MAY
  auto-refresh (debounced, off-loop) when a forward comes up.
- **`browser.drive` implemented** on HTTP, gRPC, and CLI against this surface:
  `navigate` retargets the pane/snapshot URL, `reload` re-renders or sends
  reload to the pane tool, `back` applies where the pane tool supports it.
  With nothing configured it returns a real "not configured" error instead of
  `Unimplemented`. The MCP tool stays with the in-flight write-tools branch;
  the plugin surface lights up for free through THE-39's generic `host.call`
  dispatch once the backend answer is real (see Impact). No new catalog rows.
- **Browser import: rejected.** A spec requirement pins that thegn MUST NOT
  read installed browsers' cookie/history/session stores. Authenticated
  preview flows go through (a) the user's real browser via the existing open
  action, or (b) the pane browser's own persistent profile the user logs into
  once — credentials stay in the browser that owns them.

## Impact

- Roadmap: **AC 368/369/371** (builds directly on the forward loop), **AC
  370** (friendly hostnames — untouched, still open), **AF 399** (reuses the
  graphics substrate), **AM** (a browser tile joins the daily-driver tile
  family), **AK 445** (`browser.drive` becomes a real API capability).
- Specs: new capability **`browser-preview`**. `control-plane` is unchanged in
  text (its drive-browser sentence already assumes the verb works); the change
  makes the code match it.
- In-flight overlap: **`complete-control-surface-coverage`** (THE-39) counts
  `browser.drive`'s 501 as the depth gap and ratchets `SURFACE_GAPS` — this
  change closes the depth gap on HTTP/gRPC/CLI and must land its gap-list
  edits _through_ that change's ratchet, not around it.
  **`add-drawer-tool-registry`** (THE-11) owns drawer occupants; the browser
  pane registers as one occupant when both land, and stands alone as a center
  tab otherwise. The in-flight **MCP write-tools** branch owns MCP surface
  expansion; the `browser.drive` MCP tool stays deferred to it.
- Agent-driven browsing beyond navigate/reload/back (DOM, clicks, JS) is
  **out of scope by design**: users configure their agent's own browser MCP
  server (`[mcp_servers]` — e.g. a Playwright/CDP MCP or servo-fetch's
  built-in MCP server) and thegn emits it via `thegn mcp emit`, which already
  works today.
- No DB schema change. No new catalog rows. New action ids + a
  `docs/help/browser-preview.md` page claim them (help ratchet).

## Non-goals

- No embedded/in-process browser engine, ever, under this change.
- No cookie/history/session import from installed browsers (rejected, see
  design.md Security — this is the issue's one feature we decline).
- No MITM proxy / request-inspection surface (proxelar-shaped tooling runs
  fine as a `[[tools]]`/pins/drawer entry today; nothing to build).
- No CDP client in thegn: `browser.drive` stays at the stable
  navigate/reload/back contract; richer automation belongs to the user's MCP
  browser tooling.
- No change to `[forward]` detection or the share tunnels.
