# Design — browser preview loop

## What exists (the substrate this builds on)

- **Detection** (AC 368): `[forward] auto = true` runs an off-loop `ss` probe +
  diff inside the worktree's sandbox netns and reports newly-bound ports.
- **Forwarding** (AC 371): a userspace exec-bridge proxy binds the container
  port on the host's loopback (`[forward] bind`), auto-remapping on conflict.
- **Opening** (AC 369): System ▸ Forward's `o` runs the ladder
  `[forward] browser` → `$BROWSER` → OS opener (`config.rs` `ForwardConfig`,
  `browser` field), optionally automatic (`auto_open`).
- **Graphics**: `preview_pane.rs` fetches + rasterizes off-loop
  (`spawn_blocking`) and `preview_gfx.rs` overlays the raster via the **kitty**
  graphics protocol; non-kitty terminals get text. Sixel/iTerm are AF 399's
  open tail — this change reuses the chokepoint, it does not extend protocols.
- **`browser.drive`**: a catalog row on every surface
  (`capability.rs` `"browser.drive"` / `Verb::DriveBrowser`, `SurfaceSet::ALL`)
  with a stable wire contract (`control::BrowserCommand { session, action }`,
  `BrowserAction::{Navigate{url}, Reload, Back}`) — and one backend answer:
  `Err(ControlError::Unimplemented("drive-browser"))`
  (`daemon/service.rs:694`). `cmd/session.rs` already surfaces the server's
  verdict for the CLI verb.

## The lane judgment

A terminal app has exactly three honest ways to show a web page, and thegn
already has infrastructure for two of them:

| Option                                                                  | Interactivity      | Cost                               | Verdict                                |
| ----------------------------------------------------------------------- | ------------------ | ---------------------------------- | -------------------------------------- |
| External browser (exists)                                               | full               | zero                               | keep; stays the default                |
| Terminal-browser **pane** (`carbonyl`, zenbu `terminal-browser`, `w3m`) | full, in-workspace | a config key — it is a PTY program | **in lane**: panes are the product     |
| Headless **snapshot** → kitty overlay                                   | none (a picture)   | one provider seam                  | **in lane**: reuses `preview_gfx`      |
| Embedded engine (Superset's answer)                                     | full               | a browser engine in-process        | **out of lane**: spec-pinned SHALL NOT |

The embedded engine is rejected on architecture, not taste: it would be the
first non-PTY interactive surface (breaking "panes are PTYs owned by the
daemon"), the largest dependency in the tree by an order of magnitude, a
second rendering substrate beside the cell compositor, and a permanent
security-patch treadmill. Terminal browsers deliver the same "page inside the
workspace" as an ordinary contained pane: carbonyl and zenbu's
terminal-browser are themselves Chromium-offscreen-to-terminal programs — the
engine lives in _their_ process, spawned, sandboxed, and reaped like any tool.

## Snapshot provider seam (`[browser.snapshot]`)

Standard `thegn_core::seam` shape:

- **Trait** (object-safe, in `thegn_core`): `SnapshotProvider` with
  `fn plan(&self, url: &str, out: &Path) -> Result<SnapshotPlan, SnapshotError>`
  — pure command-line planning (template render, quoting, arg assembly) so the
  core side is unit-testable to the 95% gate. Execution (spawn, timeout, PNG
  read) lives host-side next to the other subprocess seams and is
  smoke/doctor-covered, not line-covered.
- **Kinds** (`kind` implemented-or-`reserved`):
  - `none` (default) — surface hidden, zero cost.
  - `servo-fetch` — `servo-fetch {url} --format png -o {out}` (self-contained
    headless Servo; no Chromium, no API keys).
  - `chromium` — `chromium --headless=new --screenshot={out} {url}`; the
    vendor invocation (and `google-chrome`/`chrome` basename fallbacks) lives
    only inside the impl file, per the vendor-CLI rule.
  - `custom` — user template with `{url}` and `{out}` placeholders; validation
    rejects a template missing either.
- **Probe**: `thegn doctor` reports the resolved binary, version where cheap,
  and "reserved"/"not configured" honestly. The probe never fetches a URL.
- **Containment**: the snapshot subprocess is wrapped via
  `wrap_background_argv` into `thegn.slice` (same fail-safe rules as the fold
  gate — an unusable `systemd-run` runs it unwrapped), and killed on a
  `[browser.snapshot] timeout_ms` watchdog (default 10s).

## `browser.drive` delivery path

The pane/snapshot state lives in the compositor; the verb lands in the daemon.
The established daemon→compositor path is the DB `intents` mailbox —
`worktrees.open` already does exactly this (`daemon/service.rs:686`
`put_intent("focus_workspace", …)`, claimed by the model refresh within ~1s,
claim-and-delete, last-wins for focus-style intents). `browser.drive` follows
it verbatim:

- Daemon handler: validate the command, check its own config load for a
  configured surface (`[browser] pane_command` or `[browser.snapshot] kind ≠
none`) — if neither, return a real `FailedPrecondition("browser preview not
configured")` instead of `Unimplemented` — then
  `put_intent("browser_drive", <BrowserCommand json>)` and ack. The ack means
  _enqueued_, same as `worktrees.open`; ~1s claim latency is documented, not
  hidden.
- Compositor claim: `navigate` retargets the active preview URL (snapshot
  re-shoots; a pane gets the tool's own navigation where the pane command
  supports being driven, else the pane is respawned on the new URL — the
  blunt-but-honest fallback), `reload` re-shoots / respawns, `back` applies
  only where the pane tool supports it and is otherwise a no-op with a status
  line, never an error after ack.
- Config drift between daemon and compositor processes (config edited between
  their loads) can mis-answer the precondition check; accepted — the intent is
  then dropped by the compositor with a status message.

## Event loop, rendering, schema

- **Wake path:** snapshot shoots run on `crate::sched::spawn_bg` (QoS
  `Utility`), deliver `(url, raster)` over a channel, and **pulse the
  TerminalWaker** once. Auto-refresh is event-driven off the forward
  detector's existing diff (which already runs off-loop on its own cadence) —
  debounced (default 2s) so a restarting dev server doesn't shoot per flap.
  **No new polling; the idle loop still blocks on `poll_input(None)`.**
- **Damage channels:** a completed snapshot or pane (re)spawn changes chrome ⇒
  master `dirty` (`Full`), exactly like `preview_pane` fetch completion today.
  Browser-pane output is pane output ⇒ `Panes`; it never recomposes chrome.
- **SQLite:** no schema change, no `user_version` bump. The `intents` table
  exists (v34); `browser_drive` is a new `kind` string. Pane-open persistence
  reuses the session layer like any center pane.
- **Help context key:** new page `docs/help/browser-preview.md` claims the new
  action ids (`browser-pane`, `browser-snapshot`, `browser-reload`) for the
  help ratchet. The Forward panel's context key `panel:forward` stays mapped
  to `share-and-forward.md` (which gains a one-line pointer); no new context
  key — the pane itself is an ordinary center pane with no panel context.
- **e2e:** a snapshot raster and a live pane are volatile pixels; under
  `THEGN_E2E=1` the freeze pins the snapshot surface to a deterministic
  placeholder and muse specs never launch a real pane tool.

## Security

### Browser import — evaluated and rejected

THE-13 asks to "import cookies, history, and sessions from Chrome, Firefox,
Arc, and 20+ browsers so browser panes start authenticated." This is scoped
out as a **SHALL NOT**, not deferred:

- **It is a credential-exfiltration surface by construction.** A browser's
  cookie store is ambient authentication for every site the user is logged
  into. Copying it into thegn state means every pane, agent subprocess, and
  queue hook that can read thegn's state dir can impersonate the user
  anywhere. The "20+ browsers" framing describes scraping libraries whose
  other users are infostealers.
- **Keychain custody is a categorical expansion.** Chromium/Firefox encrypt
  cookie stores against OS keychains; importing means thegn asks the keychain
  for the _browser's_ secrets. thegn's credential posture today is
  SecretRef/env/file indirection for its own tokens — custody of another
  application's session keys is a different product.
- **Sandboxed panes make it worse.** The pane surface exists precisely to run
  possibly-untrusted dev tooling contained; seeding those sandboxes with the
  user's live sessions inverts the containment.
- **The linked precedent doesn't ship it either.** Superset's docs describe an
  embedded pane the user logs into, plus an opt-in "Browser Use" path that
  _attaches to the user's running Chrome_ behind explicit consent — not a
  cookie-store import.
- **If it is ever revisited**, the floor is: explicit per-profile, per-site
  consent; read-only extraction the OS keychain mediates visibly; and never
  into a sandboxed pane without an explicit policy grant. Nothing in this
  change builds toward it.

Authenticated preview instead goes through (a) the user's real browser via the
existing open action — credentials never leave the browser that owns them — or
(b) the pane browser's **own** profile the user logs into once.

### Pane profile custody

- Profile dirs live under `$XDG_STATE_HOME/thegn/browser/<scope>/`, exposed to
  the pane command via `{profile_dir}` template placeholder and
  `THEGN_BROWSER_PROFILE_DIR`; never a path into the user's real browser
  profile.
- `profile_dir_mode = "per-workspace"` (default) bounds the blast radius: a
  login made while previewing repo A is not present in repo B's pane.
  `"shared"` is documented as the trade it is.
- A logged-in pane profile is credential material readable by whatever runs in
  that pane's sandbox — stated plainly in `config.toml.example` and the help
  page.

### Drive + snapshot surface

- `browser.drive` is gated by the catalog's `required_scope(Verb::DriveBrowser)`
  on every surface — no second policy table; mutating calls ride THE-39's audit
  records once that lands.
- `navigate` makes the host fetch and rasterize an arbitrary URL — an SSRF
  shape when driven via the control API. Default confinement: targets must
  match an active forward's origin or loopback; `[browser]
allow_external_urls = true` opts out. The check is pure core logic
  (unit-tested).
- Snapshot fetches carry **no cookies and no ambient identity** — the provider
  is a cold headless fetch by definition.
- `pane_command` / `[browser.snapshot] command` are exec surfaces resolved
  from config; they follow the config-trust story
  (`add-config-trust-resolution`) like every other configured command, and the
  rendered argv is template-substituted with shell-quoted values, never
  string-interpolated into `sh -c`.

## Alternatives considered

- **Embedded engine (webkit2gtk/CEF/Servo-embed)** — rejected above.
- **A thegn-owned CDP client** driving a real headless Chrome for interactive
  render-to-terminal — rejected: duplicates what carbonyl/zenbu already are as
  plain PTY programs, and puts a CDP protocol surface inside thegn for no new
  capability. Agent automation belongs to the user's own MCP browser tooling
  (`[mcp_servers]` + `thegn mcp emit` work today; servo-fetch even ships an
  MCP server).
- **MITM/request inspection (proxelar)** — a fine `[[tools]]`/drawer occupant
  today; nothing to build.
- **Snapshot via the share tunnel** — shooting through `[share]` URLs would
  leak preview traffic off-host; snapshots always target the local forward.

## Open questions

- Should `auto_open` (existing) learn a third value `snapshot` (open nothing,
  shoot the panel snapshot) — or stay boolean with snapshot auto-refresh as
  its own `[browser.snapshot] auto = true`? Leaning the latter (orthogonal
  knobs).
- Does `browser-pane` deserve a default keybind, or palette-only until the
  drawer registry gives it a home? Proposal says palette-only.
- Snapshot image retention: keep the last raster per worktree on disk for
  instant redraw across restarts, or in-memory only (current preview_pane
  behavior)? In-memory only until someone asks.
