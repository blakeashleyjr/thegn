# Embedded agent integration — design

Date: 2026-06-22
Status: Phases A–D landed (embedding seam, proxy model path + virtual keys,
sandbox tool boundary, notifications + spend observability)

## Context

superzej's agent subsystem is a **native, embedded coding harness in Rust** —
`termite-agent` (the `apps/termite-agent` git submodule), which draws its
decisions from the pi harness (`github.com/earendil-works/pi`) and other strong
harnesses. This is a deliberate divergence from the original roadmap framing
(group **R**: adapt to _external_ harnesses via ACP/adapters). superzej ships its
own harness first; foreign-harness adapters become an additive, secondary path.

`termite-agent` already exists and is advanced (its own `docs/ROADMAP.md`,
through a Phase 4 autonomous-coding MVP). superzej's job is the **substrate** the
harness plugs into. Sequencing is **substrate-first**.

## The harness, at a glance

- Crates: `termite-core` (message/prompt/tool/provider/runtime/mcp/memory/skills/
  subagent/policy/process/context), `termite-store` (SQLite + FTS5), `termite-cli`.
- `AgentRuntime` (termite-core) is **synchronous/blocking**: its
  `OpenAICompatibleProvider` uses `reqwest::blocking`, and tool execution
  (`ToolRegistry::execute`) is synchronous. The runtime and its components are
  `Send` (provider/tool traits are `Send + Sync`).
- The provider reads `OPENAI_BASE_URL` / `OPENAI_API_KEY` / `OPENAI_MODEL` from
  the environment — which is exactly superzej's proxy seam.

## Decisions

### 1. The `AppTile` lives in the host, not the submodule (for now)

The `agent` tab is implemented as `superzej-host/src/apps/agent.rs` (`AgentUi:
sz_kit::AppTile`), depending on `termite-core` as a path dep. We did **not**
relocate the tile into `termite-agent` (the way `chat` lives in `termite-chat`),
because termite-agent has no `sz-kit` dep and no UI library crate (its TUI lives
in the `termite-cli` binary). Hosting the tile keeps the submodule a clean
library dependency and gets the tab live with minimal cross-repo surface.
Relocating it into the submodule for standalone reuse is a fast-follow once the
shape stabilizes.

### 2. Blocking runtime → `spawn_blocking`, fold back via `ChangeHook`

`AppTile::handle_input`/`render`/`pump` must never block (the ~0%-idle
invariant). A turn runs on `rt.spawn_blocking`, holding `Arc<Mutex<AgentRuntime>>`
for the duration (the `busy` flag serializes turns), and posts a `TurnEvent` on an
internal mpsc channel + fires the `ChangeHook` to wake the loop — the same pattern
as the chat tile. `pump()` drains the channel and folds results into the
transcript.

Known Phase-A limitations (tracked, not blockers):

- No mid-turn cancellation (the blocking provider call isn't abortable); `Esc`
  while idle leaves the tab, while busy it's swallowed.
- Tool-progress is returned in one batch when the turn completes
  (`run_autonomous_turn` is monolithic). Live per-step streaming needs a
  step/callback API in termite-core — harness-core work.
- Real end-to-end tool-calling needs the tool schemas wired into the provider
  request (termite-core currently sends an empty `tools` array only when
  `TERMITE_ENABLE_OPENAI_TOOLS` is set) — also harness-core work.

### 3. Proxy as the model path — LANDED (Phase B)

When `[llm_proxy].enabled`, `build_provider` points the OpenAI-compatible base URL
at `http://{llm_proxy.listen}/v1` (the local `szproxy`) and **mints a per-worktree
scoped virtual key** (`mint_proxy_key` → `db.put_proxy_virtual_key`, scope
`worktree:<path>`), using it as the provider key — the master key never reaches
the harness. The proxy authenticates by the bearer token (which _is_ the key id;
`resolve_identity` → `proxy_virtual_key`) and attributes spend to the scope. The
key is **revoked on tab teardown** (`AppTile::shutdown` → `revoke_proxy_virtual_key`).
When no model is configured, a stand-in `UnconfiguredProvider` returns a
descriptive error on the first turn, so the tab always opens (chat parity).

### 4. Sandbox as the policy boundary — LANDED (Phase C)

pi (and termite) ship **no permission system** and defer isolation to containers;
superzej's `sandbox.rs` is that missing layer. The host registers its own
`SandboxTerminalTool` (impl `termite_core::Tool`, same name/schema as termite's
`terminal`) in place of the built-in, so arbitrary commands run through the
worktree's sandbox via `sandbox::enter_argv` — resolved once at tile build by
reusing `crate::agent::prepare_sandbox` (+ the ssh-config shim). `read`/`write`/
`search` use termite's worktree-scoped built-ins. No submodule change: the host
composes the registry from termite-core's public `Tool` API. When sandboxing is
disabled, the terminal tool falls back to a host shell.

### 5. Notifications + observability — LANDED (Phase D)

Turn completion/failure publish `Event::AgentDone` / `Event::AgentFailed` into the
host `EventBus` (`publish_with_notification` → priority model + toast), threaded to
the tile via `ensure_app_loaded`. The tab chip shows a working indicator
(`AppTile::title` → `agent ●`), and `status_line` surfaces the worktree, turn
count, and **live proxy spend** (`db.proxy_budget("worktree:<path>")` →
tokens + USD, refreshed per turn). termite-store remains the transcript source of
truth; richer transcript/fleet views are group-S follow-ons.

## What landed

- Phase A: `crates/superzej-host/src/apps/agent.rs` (`AgentUi` tile), `termite-core`
  path dep, `agent` wired into `apps/mod.rs`/`run.rs`/`config.rs` + example.
- Phase B: `build_provider`/`mint_proxy_key`/`random_token` + shutdown revoke.
- Phase C: `SandboxTerminalTool` + `build_tool_registry`/`resolve_tool_sandbox`;
  ssh shim extracted to `crate::agent::apply_ssh_config_shim` (shared).
- Phase D: `EventBus` threaded through `ensure_app_loaded`; `notify_turn`,
  `refresh_spend`, spend in `status_line`.
- Tests: tile input/submit/escape, notification emission, sandbox terminal tool,
  spend display; host 610 + core 703 unit tests green.

## Verification

- `cargo build -p superzej-host`; the `agent` tab appears (per `tab_order`,
  `Alt+<n>`) and opens. With no key/proxy, a submit shows the error in-transcript.
- With `[llm_proxy].enabled` + `szproxy` running and `OPENAI_API_KEY` set, a turn
  crosses the proxy (proxy logs).
- `cargo test -p superzej-host apps::` and `cargo test -p superzej-core --lib
config::tests`.
