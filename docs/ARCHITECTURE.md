# thegn architecture

The single source for how thegn is put together and which invariants are
enforced. `CLAUDE.md` and `openspec/config.yaml` point here; the behavioural
contracts live in `openspec/specs/<capability>/spec.md`. Each section ends
with the **gate** that fails when the invariant is broken — an invariant with
no gate is a wish, not an architecture.

## 1. Crates and dependency direction

```
thegn-host  ── compositor: tokio runtime, portable-pty, termwiz Surface, chrome,
   │           daemon client/server, CLI verbs, the forge/git handles
   ├─► thegn-svc ── service seams: git (gix → CLI), forge (octocrab → gh),
   │      │         CI, issue trackers, calendar, control API adapters,
   │      │         providers, plugin transport
   │      └─► thegn-core ── substrate-agnostic domain logic: config, DB,
   │                        keymap registry, theme, termcaps, sandbox tables,
   │                        seam vocabulary, capability catalog, plugin wire,
   │                        forge/CI models + parsers
   ├─► thegn-media, thegn-metrics ── per-OS leaf crates (core-free)
   └─► tg-kit + gtui-* ── the embedded-app contract (AppTile) and the Observe app

thegn-proxy ── OPT-IN model-proxy daemon (binary `tgproxy`): the axum I/O shell
   └─► thegn-core   around `thegn_core::proxy`'s pure routing logic. Its own
                    process, NOT a dependency of thegn-host — the AI-free shell
                    never compile-depends on it (see §Background processes).
```

Rules: `thegn-core` never depends on a substrate (tokio, termwiz, portable-pty,
reqwest, octocrab, axum, alacritty_terminal, gix); each substrate has exactly
the owner crates listed in `crates/thegn-core/tests/crate_boundaries.rs`.
`vt100` and `russh` are banned outright (`deny.toml`) — both are names from
plans that never landed.

**Gate:** `crate_boundaries.rs` (`just test`), `cargo deny check bans`
(`just deps-audit`).

**Background processes (two, both per state dir).** thegn owns at most two
background daemons, each with its listen endpoint as its single-instance lock:
the **pane daemon** (`crates/thegn-host/src/daemon/`, default-on) owns PTYs so
sessions survive UI detach; the **model proxy** (`crates/thegn-proxy`, binary
`tgproxy`, OPT-IN via `[model_proxy]`) is the local dual-protocol model endpoint
— tier routing, ordered failover, cost/budget accounting. The proxy is a
separate process precisely so the AI-free shell never compile- or run-depends on
it: with `[model_proxy]` absent nothing launches, spawns, writes, or renders.
The host supervises it off the UI loop (`model_proxy_daemon.rs`, `core::backoff`
restarts) and projects control through four OPERATOR-only capability rows
(`model_proxy.status/stats/start/stop`). The pure routing logic lives in
`thegn_core::proxy` (95%-gated); the resurrection boundary is firm —
ACP/bouncer/tool-interception, token compression, remote-sandbox tunnels, and
the managed-agent dialer stay excised.

## 2. The event loop (0% idle)

When idle the loop blocks on termwiz `poll_input(None)` — no tick, no timeout.
While work is already in hand (a dirty frame, queued input, an exhausted
frame budget) it polls with an 8 ms batching timeout; a gate-deferred frame
arms its exact remainder. That decision is the pure function
`thegn-host/src/idle_poll.rs::poll_timeout`. Every off-thread producer (PTY
readers, hydration on `spawn_blocking`, fs-watchers, the 2 s ticker) sends on
a tokio mpsc channel **and pulses the `TerminalWaker`**; the loop drains on
wake. Never put blocking I/O on the loop — and the launch path before the first
frame runs no synchronous subprocess I/O either. The two startup git jobs —
the main-checkout heal (`startup_heal::spawn`, over the launch dir, each
session worktree group and the canonical checkout) and the merge-sweep's repo
root resolve — run off the loop: the heal on its own named `Background`-QoS
thread, the sweep's resolve inside its existing `spawn_blocking` task. The
heal's completion is
a bounded barrier (`startup_heal::HealGate`, `BARRIER_TIMEOUT_MS`) that the
first git-reading consumer (the initial model hydration) awaits, so a stray
`core.worktree` can never poison a hydration pass; a healed checkout pulses
one `RefreshKind::Model` + waker. The remaining sanctioned on-loop subprocess
sites are interactive and post-frame (`git init` on explicit user confirm,
documented at the site) or not the loop at all (`src/cmd/` CLI verbs, work
already inside `spawn_blocking`/threads) — the host `clippy.toml`
`disallowed-methods` gate plus local `#[expect]`s with reasons is the
enforceable form of this rule.

**Gate:** `idle_poll` unit tests; `just lint` asserts exactly one timed
`poll_input` site (`run.rs`) and that every other call is `None` or a
zero-timeout drain; `render_plan::plan` tests lock the render decision
(`Skip` / `Panes` / `Full`); thegn-host clippy.toml disallowed-methods
(blocking child waits) with local expects at sanctioned off-loop sites.

## 3. Rendering and terminal degradation

A damage-region compositor (`render_plan.rs` + the `run.rs` render block):
`full` (geometry), `chrome` (sidebar/panel/bars/overlays/model) and
`dirty_panes` (per-pane PTY content) map to `Skip`, `Panes` (bounded diff) or
`Full`. The frame is always composed in truecolor + Unicode; degradation
happens at two chokepoints — color quantizes truecolor→256→16→mono in
`wire.rs::color_spec`, glyphs swap Unicode↔ASCII via `caps::active_glyphs()`.
Capabilities come from `thegn_core::termcaps` (pure, env-driven, optional
DA/XTVERSION probe) folded with `[theme] color`/`glyphs`.

**Gate:** `test/color-literal-ratchet.txt` and `test/glyph-literal-ratchet.txt`
(no literals outside the chokepoints, shrink-only); `just term-check` (six
terminal environments through `thegn doctor`) in `just ci`.

## 4. Platform code

Per-OS code lives in `thegn-host/src/platform/`, the `termcaps`/`sandbox*`
tables in core, and the two per-OS leaf crates. A `#[cfg(unix|windows|
target_os…)]` anywhere else is debt.

**Gate:** `test/platform-cfg-<crate>-ratchet.txt` ×5 (shrink-only);
`just check-cross`, `just check-features`, `just check-msrv` in `just ci`.

## 5. Provider seams

Every substitutable backend is a _seam_ (`openspec/specs/provider-seams`):

| Seam             | Trait                                         | Impls                                                                           | Selected by                                              |
| ---------------- | --------------------------------------------- | ------------------------------------------------------------------------------- | -------------------------------------------------------- |
| forge            | `thegn_core::forge::Forge` (sync)             | GitHub: `GithubNative` → `GithubCli` ladder                                     | `[[forges]]` by origin host; GitHub default              |
| git              | `thegn_svc::git::GitBackend`                  | `GixGit` (native reads) → `CliGit`                                              | `[git] backend` (`auto`/`gix`/`cli`)                     |
| CI               | `thegn_svc::ci::CiProvider`                   | GitHub Actions, GitLab CI                                                       | `[ci] provider` (Drone/Woodpecker/Jenkins/Argo reserved) |
| editor           | `thegn_core::editor::Editor`                  | template (`[editor] command`) → `[[tools]] editor` → `$VISUAL`/`$EDITOR` → `vi` | `[editor]`; placement per program or `open_in`           |
| issues           | `thegn_svc::issue::IssueBackend`              | Linear, GitHub, Jira, Kaneo                                                     | `[[issue_accounts]]`                                     |
| calendar         | `CalendarBackend`                             | ICS, ICS URL, CalDAV, command (plugin)                                          | `[[calendar_accounts]]`                                  |
| media            | `thegn_media::MediaBackend`                   | MPRIS, playerctl, mpv, MPD, SMTC, MediaRemote, AppleScript                      | `[media] backend` (Jellyfin reserved)                    |
| sandbox          | `thegn_core::sandbox::Backend` (enum)         | podman, docker, bwrap, systemd, apple, WinAppContainer, WinJobObject, none      | `[sandbox] backend` / `backend_chain` (WSL reserved)     |
| remote providers | `RemoteProvider` (+ egress/checkpoints/files) | Daytona, Sprites, VPS (Hetzner/DO), Fly, Machine0, Iroh                         | `[env.<name>.provider]`                                  |

The shape: **object-safe trait, always** — plain `&self` when every impl is
process-bound (forge, git, CI, sandbox, editor), `fn m<'a>(…) ->
BoxFuture<'a, R>` when callers are async (control API, issues, calendar,
media, remote providers). Never `async fn` in a seam trait, and never a
hand-written per-method delegation enum — dispatch is `Box<dyn T>` /
`&dyn T` accessors, so a new provider is a registration, not N match-arm
edits. Then: `caps()` ⇔ optional ops defaulting to `Unsupported`, a
`SeamError` that classifies for `Ladder` fall-through
(`Unsupported`/`NotInstalled`/`NotConfigured` fall through; `Auth`/
`NotFound`/`Transient` are final), `Probe` → `thegn doctor`'s Providers
section, and a `config_enum!` kind where every value is implemented or
`reserved` (`config validate --strict` rejects reserved) — or, for
account-shaped seams (issues, calendar), an open `[[…_accounts]]` list whose
factory builds `Some` exactly for implemented kinds. Vendor CLIs are called
only inside their implementation files. Account routers accept
dynamically-provided backends (`push_backend`) — how provider-as-plugin
composes (§6).

**Gate:** `test/forge-leak-ratchet.txt` (4 impl files), `kind_coverage` per
seam, `test/async-trait-ratchet.txt` (**empty and stays empty** — clippy's
`-D warnings` catches a bare `async fn` in a public trait, and the allow
that silences it is a ratchet violation), `thegn_svc::conformance` (probe
shape, reserved reporting, account-factory coverage, determinism),
`registry::probes` in `thegn doctor` (asserted by `test/smoke.sh`).

## 6. External doors: one capability catalog

The control API (HTTP/WS/SSE + gRPC), the `thegn` CLI control verbs, the MCP
server and plugin `host.call`s are projections of
`thegn_core::capability::CATALOG` — one row per control `Verb`, each with a
stable id, the scope `required_scope(verb)` demands, and the surfaces it is
exposed on. HTTP routes are _built_ from `thegn-svc/src/control/routes.rs`,
and its `API_CALLS` table (`cap`, method, path — pinned against `ROUTES`)
is the generic-client spine: `thegn api list|schema|call` performs any
routed capability with **no per-verb client code**, `cli_control_caps()`
derives the CLI surface from it, and the control wire types are pinned by
`docs/api/control-v1.json`. `thegn mcp serve --scopes` adds state tools
(`mcp::state::StateRouter`, scope-gated) beside the docs tools. What a
surface does not implement yet is an entry in `SURFACE_GAPS`, which only
shrinks — pinned shrink-only by `test/surface-gaps-ratchet.txt`
(`ratchet_pins_surface_gaps`): adding an excuse fails the build until the file
grows a line, and the terminal state (empty table) is pinned empty. A surface a
capability is _deliberately never_ exposed on (pairing management + shutdown are
HTTP + CLI only) is expressed by narrowing the row's surface set, never by an
excuse. Each surface's implemented set is one table (`API_CALLS`, `GRPC_CAPS`,
`cli_control_caps()`, `MCP_STATE_CAPS`, `plugin_host_call_caps()` — derived from
the catalog) that `coverage_problems` arbitrates; `thegn api coverage` prints
the per-surface ledger (implemented / **stub** / excused / declared — a
routed-but-inert row like `browser.drive` carries a `stub` marker so it never
reads as done) and `thegn doctor` the one-line summary. Plugin `host.call`
dispatches any routed non-streaming plugin row generically through the
`API_CALLS` spine; the event feed is bridged to resident plugins as `on_event`.
Every mutating control call and every auth/scope rejection emits a structured
record on the `thegn::control::audit` tracing target (`thegn_core::control_audit`;
never a token secret).

The CLI's `thegn events tail` is the reference thin client for the streaming
`events.subscribe` projection. It uses the existing discovered Unix socket or
the existing bearer-authenticated TCP endpoint, and accepts only narrowing
filters (`--kinds`, `--session`) plus opt-in bounded-loss signaling
(`--signal-lag`). It has no replay journal: a `lagged` frame or reconnect tells
the consumer to resynchronize through `sessions.list` and `worktrees.list`.
The command is read-only and does not enable the `sessions.input` interlock
(`--allow-session-input`).

**Plugins** speak the NDJSON wire in `thegn_core::plugin_api`, versioned by
`API_VERSION` and pinned by `docs/api/plugin-api-<v>.json`, and are _run_ by
the plugin runtime (`openspec/specs/plugin-runtime`): loader (`[[plugins]]`

- `plugins/*/plugin.toml`), one-shot and resident modes, statusbar/palette/
  notification surfaces, scope-checked `host.call`, and provider-as-plugin —
  an `IssueProvider` contribution is bridged onto the issue seam over
  `provider.call` (`ProviderBridge` correlation + per-plugin timeout) and
  joins every `IssueRouter` beside configured accounts. Nothing plugin-side
  ever touches the idle loop (channel + waker, like every producer).

**Gate:** catalog tests (`every_verb_has_exactly_one_row`, admin never on
MCP/plugin), per-surface `coverage_problems` tests (HTTP, gRPC, CLI, MCP,
plugin), `api_calls_mirror_routes`, `tests/plugin_api_wire.rs` +
`tests/control_schema.rs` snapshots, the `examples/plugins/hello.sh` golden
test, and `thegn plugin check` / `thegn plugin list` in `test/smoke.sh`.

## 7. Configuration

Rust structs are the schema (`schemars` → `thegn config schema`, the strict
validator, the MCP resource). `config/config.toml.example` documents every
key and is the source of the runtime-generated config-reference help page.
Layering: built-in defaults → `$XDG_CONFIG_HOME/thegn/config.toml` →
`THEGN_<SECTION>_<KEY>` env → `--set`; a repo's `.thegn.toml` overlays
`[sandbox]` only. Unknown keys are dropped on load (a launch is never blocked)
and rejected by `thegn config validate --strict` with a did-you-mean.

**Gate:** `tests/config_example.rs` (every key documented; example parses and
validates clean), `tests/env_overlay_coverage.rs` (every shallow key has an
env knob or is pinned in `test/env-overlay-ratchet.txt`; every knob is
exercised), `tests/hm_module_drift.rs` (the home-manager module renders only
real keys and offers only accepted enum values), the `config_enum` pinned
count.

## 8. Keymap, palette, help

`ACTION_SPECS` (`thegn-host/src/keymap_specs.rs`) is the action registry:
every `Action::key()` id has a spec (label, keywords, default chords, palette
flag); `keymap_merge::collect` folds specs + zone key tables + user rebinds
into the one list `thegn keys list` and the generated keybindings help page
share. The command palette is spec-driven — there are no string-keyed verbs.
Every action id is claimed by a `docs/help/` page that actually mentions it.

**Gate:** `every_action_key_has_a_spec_and_round_trips`,
`declared_default_chords_actually_dispatch`, `every_palette_key_is_an_action`,
the three help ratchets (`test/help-*-ratchet.txt`),
`every_help_page_is_registered`, `cli_help::GROUPS` drift test.

## 9. State

SQLite at `$XDG_STATE_HOME/thegn/thegn.db` (WAL, `user_version`-versioned,
ladder-tested migrations): repos, workspaces, worktrees, PR cache, layouts,
UI state. **git is the source of truth** for worktrees; the forge is the source
of truth for PRs; the DB is a cache + resurrection layer. Ignored `Result`s
on cache writes are the sanctioned best-effort pattern and are marked
`// best-effort:`.

**Gate:** `db_tests` migration ladder; `let_underscore_future = deny`;
`test/ignored-result-ratchet.txt`.

## 10. Sandboxing

A worktree's interactive process can run in a container (podman → docker →
bwrap → none, or the Windows/Apple native backends) with the worktree
bind-mounted at its real path so host-side git keeps working; remote
backends run it on another machine. Backend availability is a three-state
probe (present / absent / unreachable) so an undecidable remote halts rather
than silently degrading.

**Gate:** `openspec/specs/sandbox` (32 requirements), `just sandbox-e2e-dns`
/ `-db` in `just ci`.

## Adding things

Step-by-step recipes, each ending with the gate that fails if you skip a
step: `docs/extending/`.
