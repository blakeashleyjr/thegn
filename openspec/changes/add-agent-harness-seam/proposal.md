# Add the agent harness seam (session history, resume, usage, scoped control)

Linear: THE-31

## Why

thegn already knows a great deal about each coding-agent CLI ("harness") — but
that knowledge is scattered across at least six vendor-hardcoded sites, and
whole facets have no home at all:

- **Identity/login**: `thegn_core::account::PROVIDERS` (id, `home_env`,
  `default_dir`, `login_argv`, `auth_marker`).
- **Headless launch forms**: the `match provider` in
  `thegn_core::agent_task::headless_command` (`claude -p … --permission-mode
acceptEdits`, `codex exec …`, `aider --yes --message …`).
- **Bare-provider resolution**: `thegn-host/src/daemon/agent_open.rs::bare_provider`.
- **Usage/limits parsing**: `thegn_core::usage::parse_claude_usage` /
  `parse_codex_rollup` — one hand-written parser per vendor.
- **Session-store layout**: `thegn-svc/src/usage.rs` walks Codex
  `sessions/rollout-*.jsonl` and Claude `projects/**/*.jsonl` — but only to
  count tokens; the sessions themselves are discarded.
- **Login-sync allowlists**: the sandbox credential-carry knows which files are
  auth-critical per harness.

Adding a new harness (Gemini CLI, OpenCode, Copilot CLI …) is an N-site sweep
today, and three facets users expect from comparable tools are missing
entirely: **session history** (toktrack unifies eight CLIs' session stores;
codeg aggregates cross-harness conversation history per project and resumes in
the original agent), **resume** (relaunch a specific prior session — today
resurrection always starts the agent cold), and **scoped protocol control over
thegn itself** (which capabilities an agent-facing MCP server exposes, decided
per project / profile / global rather than per invocation).

## What Changes

1. **A `Harness` provider seam** (`thegn_core::harness`, following
   `thegn_core::seam` conventions): object-safe trait, caps ⇔ optional ops, a
   **closed registry** (an unknown harness id is an error or `reserved`, never a
   guessed command), and a `Probe` per configured harness in `thegn doctor`
   (binary on PATH, credential home present, auth marker, session store found).
   Required ops: identity + home resolution, interactive command, headless
   template, login form. Optional ops (capability-gated): **SessionStore**
   (discover local session transcripts), **Resume** (argv form to resume a
   session id), **Usage** (parse local rate-limit/usage state; opt-in live
   fetch), **Tokens** (transcript token fold), **Teammates** (reserved — see
   design.md open questions).
2. **Retrofit, behavior-identical**: the `claude` and `codex` implementations
   (plus `aider` headless and `antigravity` usage) absorb the six scattered
   sites; `account::PROVIDERS`, `agent_task::headless_command`,
   `usage::parse_*`, the `thegn-svc` usage walkers, and
   `agent_open::bare_provider` delegate to the seam. Vendor strings appear only
   inside the implementation files (the seams-not-vendors ratchet).
3. **Session history**: a new `agent.sessions` capability row (Read) listing
   discovered harness sessions — harness, session id, worktree/project, mtime,
   one-line summary — projected across HTTP/CLI/MCP (`thegn agent sessions
--json`, MCP `agent_sessions`). Read-only; never spends tokens; never
   includes credential material.
4. **Resume / auto-resume**: `AgentLaunch` gains a `resume` field (open a
   session continuing a prior harness session); a per-`[[agents]]` `resume`
   config key lets session resurrection relaunch the remembered agent _resuming
   its latest session for that worktree_ instead of cold. Explicit resume of an
   unknown id errors; auto-resume falls back to a cold launch.
5. **Scoped MCP control over thegn**: the scope set `thegn mcp serve` grants is
   resolved from config — global, then the active profile, then the workspace
   overlay, each able only to **narrow** the outer level — with `--scopes` as a
   final narrowing override. This depends on the in-flight MCP write-tools work
   (parameterised state tools + `--scopes` gate) and on
   `add-config-trust-resolution` for the trust model of repo-local config.

Every new externally invokable operation is a `thegn_core::capability::CATALOG`
row gated by `required_scope(verb)`, projected (or a recorded `SURFACE_GAPS`
entry) on every surface — no second policy table.

## Impact

- **Roadmap**: Q 658 (agent session history — the list/resume half), R 240/242
  (top-N harness support / per-harness capability detection — realized as seam
  impls + caps, not ACP), P 200 (harness adapter plugins — the seam is the
  trait they would implement), S 257 (transcript viewer — discovery lands here,
  viewing stays future), V 300 (the shipped usage tracker refactors under the
  seam, behavior-identical).
- **Specs**: `agent` — ADDED (harness seam, session history, resume), MODIFIED
  (worktree agent memory gains auto-resume). `control-plane` — MODIFIED (MCP
  scope-gated state tools gain config-resolved scope ceilings).
- **In-flight changes**: depends on the MCP write-tools branch (scoped state
  tools; do not re-scope it) and `add-config-trust-resolution` (clamping rules
  for repo-local scope config). Coordinates with `add-agent-task-engine`
  (headless templates move behind the seam; the engine's resolution order is
  unchanged) and `make-daemon-default` (agent launches increasingly route
  through the daemon). Sibling scoping: `add-agent-orchestration-surface`
  (THE-57) consumes the same launch path; `make-usage-overlay-scannable`
  (THE-65) presents what the Usage op gathers.
- **AI-free shell**: strictly additive. With no harness configured, nothing
  changes; every op degrades to "capability absent".
- **No DB schema change.** Session discovery is a filesystem read; the DB may
  cache it later, but that is not this change.
