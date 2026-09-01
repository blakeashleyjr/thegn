# THE-13 architecture: worktree frontend preview loop

Status: implementation design for `tg/the-13-browser-preview`.

## Outcome

Deliver a low-cost preview loop for frontend work in the active worktree:

1. discover a candidate dev-server port from pane output, explicit preview
   config, or package-manager scripts;
2. expose the candidate as a live preview target with `up`, `down`, or
   `unknown` status and its port/URL in the existing system/forward view plus a
   compact sidebar/drawer token;
3. keep the existing `o` external-browser opener;
4. make an optional terminal-browser occupant available through the THE-11
   drawer-tool registry, with the preview URL supplied as runtime context;
5. expose one read-scoped `preview.fetch` capability that performs a bounded,
   cookie-free localhost HTTP GET and returns the page body plus diagnostics
   captured from the dev-server pane.

The deliverable is a preview loop, not a browser product. It does not import
browser data, link a browser engine, render screenshots, drive arbitrary browser
automation, inspect traffic, or add a new pane kind.

## Branch-grounded baseline

The openspec draft was read first, then checked against this branch. Its claims
fall into three categories:

| Draft claim                                 | Branch evidence                                                                                                                                                                                                                                                                                                          | Decision                                                                                                                                                              |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Sandbox port detection and forwarding exist | `thegn_core::forward` already has pure `ss` parsing and diffing (`crates/thegn-core/src/forward.rs:1-137`); host detection is a background thread and pulses the waker (`crates/thegn-host/src/forward.rs:276-381`); host forwarding/proxying is `ForwardSupervisor` (`crates/thegn-host/src/forward.rs:79-190`)         | Reuse the forward transport and its existing off-loop event source. Do not add another preview probe or idle poll; lifecycle after detection is watcher/child driven. |
| External browser opening exists             | The Forward panel is rendered from `FrameModel::forwards` (`crates/thegn-host/src/panel/sections/misc.rs:266-303`), and `o` invokes `[forward].browser` or the opener ladder (`crates/thegn-host/src/run.rs:18068-18086`)                                                                                                | Keep it as the default full-browser path.                                                                                                                             |
| A web snapshot/graphics path exists         | `preview_pane` and `preview_gfx` are the local file/document preview path (`crates/thegn-host/src/preview_pane.rs:1-18`); the event loop wires file-preview channels (`crates/thegn-host/src/run.rs:5817-5824`)                                                                                                          | Do not repurpose this into web rasterization. The draft's snapshot provider and Servo/Chromium commands are pruned.                                                   |
| `browser.drive` is ready to implement       | The wire type exists (`crates/thegn-svc/src/control/mod.rs:210-224`), the HTTP route exists (`crates/thegn-svc/src/control/routes.rs:97`), but the daemon returns `Unimplemented` (`crates/thegn-host/src/daemon/service.rs:762-764`) and the catalog marks it as a stub (`crates/thegn-core/src/capability.rs:274-280`) | Leave this compatibility slot/stub unchanged. THE-13 asks for `preview.fetch`, not browser navigation/back/CDP.                                                       |
| A browser pane can be added                 | Current drawer code is file-manager-only and owns a boolean flag, pool, and async spawn (`crates/thegn-host/src/drawer_state.rs:1-20`, `:115-249`); THE-11's design makes it an ordered registry whose generic occupants are PTYs (`openspec/changes/add-drawer-tool-registry/design.md:1-25`, `:44-91`)                 | Consume the THE-11 registry context. Do not create `BrowserPane`, a browser engine, or a second command registry.                                                     |
| Browser import is part of the feature       | No browser credential-store integration exists on this branch.                                                                                                                                                                                                                                                           | Reject permanently in this design; see Security.                                                                                                                      |

The existing `[forward]` section is transport configuration, not the preview
domain model. Its current schema is only auto/range/ignore/only/bind/poll/browser
(`crates/thegn-core/src/config.rs:3681-3724`, documented at
`config/config.toml.example:2923-2940`). The new preview policy belongs in a
sibling `config_preview.rs`, not in the already large `config.rs`.

## Draft reconciliation

The four draft files were read before this design: `openspec/changes/add-browser-preview-loop/proposal.md`, `design.md`, `tasks.md`, and `specs/browser-preview/spec.md`. The proposal's claim that detection, forwarding, and external opening already exist (proposal `:7-11`) is satisfied by the branch evidence above; this design does not rebuild those paths. Its `[browser]` profile/snapshot provider, raster overlay, and `browser.drive` phases (proposal `:32-63`; spec `:20-147`) are pruned because the issue lead framing replaces them with a drawer-registry terminal tool and the bounded `preview.fetch` capability. The draft's no-engine, no-browser-import, no-MITM/CDP, and no-new-forward-detection boundaries (proposal `:95-105`; spec `:5-18`, `:53-66`) are retained. The draft's intent-mailbox and DB-backed browser-drive plan is also rejected: preview status is live memory, and fetch requires a URL plus optional worktree context rather than stale DB state.

## Domain model and source precedence

Add a substrate-free `thegn_core::preview` module. It owns pure values and
policy only:

- `PortHint { port, host, source }`, where source is `Config`, `PaneOutput`, or
  `PackageScript`;
- a bounded ANSI-tolerant parser for common dev-server output (`http://localhost:
N`, `127.0.0.1:N`, `[::1]:N`, `--port N`, `-p N`, and `PORT=N`), never a
  general URL crawler;
- a package-script parser for `package.json` `scripts.dev`/`scripts.start`
  and equivalent explicitly known start scripts, extracting only explicit port
  arguments;
- deterministic merge/deduplication and target selection;
- pure localhost/loopback URL validation for `preview.fetch` and redirect
  revalidation;
- pure bounded response/status types used by host and control layers.

Precedence is explicit config, then an observed pane-output URL/port, then a
package-script hint. An observed URL wins over a guessed host but not over an
explicit configured port. Multiple valid hints remain separate candidates;
the active target is the highest-precedence live candidate, with stable
port-order tie-breaking. A package script is a hint, never a command to run.

`PreviewTarget` is live host state, not DB state: worktree identity, port,
resolved preview URL, source, associated pane/session when known, lifecycle
status, and a bounded diagnostic ring. `up` means a discovery/forward event
has established a reachable target; `down` means the watched pane/forward
ended; `unknown` means a candidate exists but no lifecycle event has proved
reachability. A fetch failure may transition the target to `down` for the
current generation, but it never starts a retry timer.

## Event loop and detection

The current PTY pipeline already receives `PaneEvent::Output` and
`PaneEvent::Exit` (`crates/thegn-host/src/pane.rs:67-89`) through the bounded
drain (`crates/thegn-host/src/pty_drain.rs:265-299`). The preview handler will
consume the same drained output after the emulator feed, retaining only a
bounded, ANSI-stripped tail for diagnostic parsing. It sends no extra wake for
work already being drained; asynchronous work sends a channel message and
pulses `TerminalWaker`, matching the repository rule in
`docs/ARCHITECTURE.md:54-84`.

Discovery sources:

- Pane output is edge-triggered by the existing PTY reader. A pane exit/EOF is
  the down event for targets associated with that pane.
- `preview.ports` and package-script hints are read once, off-loop, when the
  active worktree changes or config reloads. Existing filesystem-watch events
  can invalidate that one-shot scan; no preview timer is added.
- A sandbox hint is handed to the existing forwarding seam off-loop. The
  forwarding provider may use its runtime's event/child FD stream; the host
  consumes `ForwardEvent` and watches the resulting proxy/placement lifecycle.
  Vendor runtime commands remain inside `thegn-svc::forward` implementations.

The existing sandbox detector at `crates/thegn-host/src/forward.rs:276-381` is
already an off-loop provider that emits `ForwardEvent` and pulses the waker.
Preview consumes that existing event source; it does not add a second probe or
read `[forward].poll_secs` as preview policy. Once a candidate is detected,
its lifecycle is watched through the PTY, provider event stream, proxy task, or
child process handle. If a placement cannot offer an event source, the target
degrades to `unknown` and explicit fetch/open remains available. The idle loop
never polls to manufacture certainty.

The event loop remains idle on `poll_input(None)` as locked by
`crates/thegn-host/src/idle_poll.rs:23-36` and renders target/status changes as
chrome/sidebar damage. Pane-browser output is only pane damage. A target
completion that changes the token or panel is chrome damage; it must not cause a
full recomposition for unrelated PTY output, consistent with
`crates/thegn-host/src/render_plan.rs:19-48` and `:130-151`.

## Surfaces

### External browser

The existing Forward row remains the external-browser affordance. Its URL is
the resolved host URL after any sandbox remap. No new opener ladder or browser
vendor integration is needed.

### Drawer terminal browser

THE-11 owns the drawer rect, registry, occupant pooling, process lifecycle,
containment, persistence, and picker. THE-13 supplies only a preview context
adapter:

- a configured `[[drawer.tools]]` entry may declare the reserved runtime role
  `preview` and reference the existing `[[tools]]` command registry;
- when selected, the registry receives `DrawerContext::Preview { url, worktree,
port }` and injects the URL as an argv value (or a single exact
  `{preview_url}` token), never through `sh -c` interpolation;
- the adapter exports `THEGN_PREVIEW_URL`, `THEGN_PREVIEW_PORT`, and
  `THEGN_PREVIEW_WORKTREE` for tools that prefer environment configuration;
- an absent or invalid preview occupant is omitted with a warning; the drawer
  still behaves as THE-11 specifies for files and other tools.

This is a serial dependency on `tg/the-11-drawer-tools`. The THE-13 code must
call the registry's public occupant/context seam and must not edit generic
pooling or file-manager behavior except where THE-11's agreed context hook is
required. If THE-11 has not landed, external opening and the status token still
ship; the center-pane fallback is explicitly not a new browser pane kind.

### Status token and panel

Extend the render model with a small `PreviewView` projection rather than
teaching the panel to inspect supervisors. The System → Forward section keeps
its existing transport rows and gains preview source/status text. The active
worktree sidebar row gets a compact token such as `preview ↑5173`, `preview
down`, or `preview ?5173`; the open drawer divider may show the same token.
All color/glyph choices go through the existing segment/theme/capability
helpers. No literals are introduced at a draw site. The preview token is
silent when no candidate exists, so an unused worktree pays no recurring work
or visual noise.

## `preview.fetch` capability

Add one read-scoped catalog row, `preview.fetch`, with `SurfaceSet::ALL`. This
is the single external identity; HTTP, gRPC, the generic CLI API, MCP state
tools, and plugin `host.call` project it rather than defining parallel verbs.
The existing `browser.drive` row remains a separate stub and is not silently
relabelled.

Request:

```json
{
  "url": "http://127.0.0.1:5173/",
  "worktree": "repo-feature",
  "include_console": true
}
```

Response is bounded and JSON-safe:

```json
{
  "url": "http://127.0.0.1:5173/",
  "status": 200,
  "content_type": "text/html",
  "body": "<…>",
  "truncated": false,
  "console_errors": [],
  "diagnostics_source": "dev-server-pane"
}
```

The daemon/host implementation uses the existing `reqwest` dependency already
owned by the host (`crates/thegn-host/Cargo.toml:103-106`), with a fresh request client
configured for this seam. It performs GET only, has a configured timeout and
maximum body size, disables cookies/auth headers and ambient proxy use, and
manually follows at most a small number of redirects while reapplying the
loopback policy to every `Location`. Default accepted authorities are
`localhost`, `127.0.0.1`, and `::1`; external URLs require the explicit
`preview.allow_external_urls = true` policy. The opt-out is documented as an
SSRF boundary and remains subject to the request/body/time limits.

HTTP fetch does not create a browser JavaScript runtime. Therefore
`console_errors` means structured error lines captured from the associated
dev-server pane's bounded output ring. `worktree` is optional for callers that
already have a target URL, but is required when selecting diagnostics for a
non-active target. The response identifies that source (or
`unavailable`); when no pane diagnostics exist (including when the control
service is not co-located with the compositor) it returns an empty list, not a
fabricated browser-console claim. Browser-console inspection and DOM/JS driving
belong to the user's configured external browser/MCP tool, not thegn.

The request carries a URL because the pane compositor and the control daemon
are separate ownership boundaries. Agents obtain it from the preview status or
forward listing; the daemon does not guess an active UI target from stale DB
state. The existing forwards table remains a cache/resurrection aid only, in
line with `docs/ARCHITECTURE.md:230-257`; no migration or new schema version is
needed.

## Configuration

Add `[preview]` as a sibling config module with these defaults:

```toml
[preview]
enabled = true
ports = []
fetch_timeout_ms = 3000
max_body_bytes = 1048576
allow_external_urls = false
```

`ports` is an explicit port hint/allowlist, not a dev-server launcher. All five
shallow keys receive `THEGN_PREVIEW_*` overlays and are documented in
`config/config.toml.example`; malformed values warn and fall back according to
the existing config layering rules (`crates/thegn-core/src/config.rs:1-14`,
`:5706-5724`). No `[browser]` table, snapshot kind, profile directory, cookie
path, or `poll_secs`-style preview timer is added. Existing `[forward].poll_secs`
is legacy forward-detector debt and must not become part of the new preview
contract; its removal/compatibility handling belongs with the forward detector
refactor and must be documented if changed.

## Security decisions

### Browser data import: rejected

THE-13's request to import cookies, history, and sessions from Chrome, Firefox,
Arc, and other installed browsers is rejected as a security boundary, not
deferred work. Thegn MUST NOT read, copy, decrypt, or request OS-keychain
material for another application's browser profile. Those stores are ambient
authentication for every site and importing them into thegn state or a pane
would turn a contained development tool into a credential exfiltration path.
Authenticated previews use the user's real browser through the existing opener,
or a terminal-browser tool's own profile/log-in flow. The preview drawer never
receives a path into an installed browser profile.

### Other boundaries

- No embedded Servo/Chromium/WebKit engine. A terminal browser is an ordinary
  user-configured PTY occupant; an external browser is outside thegn.
- No CDP client, DOM automation, JavaScript execution, MITM, request body
  inspection, or Proxelar integration. These are tool/plugin territory.
- No cookies, Authorization headers, client certificates, or inherited browser
  profile state in `preview.fetch`.
- Configured drawer commands remain trusted-config execution surfaces and must
  pass THE-11/config-trust rules. URL values are argv/env data, never shell
  source.
- `preview.fetch` is read-scoped, audited by the existing control-plane audit
  path, and bounded. A failed or unavailable target degrades to an explicit
  status/error; it never blocks the compositor.

## Draft pruning and already-satisfied work

Already satisfied on this branch: pure `ss` output parsing and port diffing;
loopback forward binding with conflict remap; active Forward panel rows; the
external browser opener; stable `BrowserCommand` wire shape and routes (but not
its behavior); generic capability/catalog coverage machinery; the file-preview
graphics path; and the file-manager drawer's off-loop spawn/waker pattern.

Pruned from the openspec draft: `[browser] pane_command`, browser profile modes,
the `SnapshotProvider` seam and `servo-fetch`/Chromium implementations, PNG
rasterization/kitty web overlay, `browser.drive` navigate/reload/back behavior,
MCP deferral for that drive verb, CDP, MITM/Proxelar work, and E2E freeze rules
for volatile web pixels. These either add an embedded/rendering substrate,
duplicate THE-11, promise browser-console semantics HTTP cannot provide, or
cross the explicitly rejected credential boundary.

## Ratchets and verification

The implementation must update or explicitly preserve, in the same chunk that
changes the relevant surface:

- `test/env-overlay-ratchet.txt` and
  `crates/thegn-core/tests/env_overlay_coverage.rs` for every new config key;
- `test/completion-slot-ratchet.txt` and the completion catalog if a dedicated
  CLI URL argument is introduced (the planned generic `thegn api call` path
  introduces no new value-taking slot);
- `docs/api/control-v1.json` through
  `crates/thegn-svc/tests/control_schema.rs` for the new capability wire types;
- `test/help-ratchet.txt`, `test/help-prose-ratchet.txt`, and
  `test/help-panel-prose-ratchet.txt` if an action/panel section is added; the
  planned change extends the existing Forward help page and adds no new
  action, so these should remain clean and unchanged;
- catalog/surface-gap ratchets, with no new excuse for HTTP/gRPC/CLI/MCP/plugin
  once `preview.fetch` is implemented on all five projections;
- affected catalog/panel test fixtures and snapshots, without running E2E in
  this architecture pass.

Each implementation test/invocation that could open the state DB must set
`XDG_STATE_HOME` to a fresh temporary directory. Never run a migration or the
built binary against the live state DB.
