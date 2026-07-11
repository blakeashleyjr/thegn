# Design

## Model

A skill is a versioned `SKILL.md`-style package: `name`, semver `version`, a
body (the injectable prose/instructions), and metadata (`triggers` — the
task/context match hints; `requires` — optional referenced MCP servers from the
central registry, AR 541; `harnesses` — optional render hints; `tools` —
optional house-tool references, AR 545). The registry resolves a _registered_
package to an _installed, pinned_ version (Orca `npx skills add` model:
register → install-on-demand → pin). Per-scope enablement (global / workspace /
worktree) lives in `ui_state`, like other gateway bindings; package bodies are
cached in the DB so install-on-demand survives offline.

## Selection + injection seam — `skills::select_and_render()`

A single core seam (`crates/thegn-core/src/skills.rs`) takes
`(task_context, harness, policy)` and returns an ordered list of rendered
capability blocks. Selection is deterministic: match a skill's `triggers`
against the task/context, filter by per-scope enablement, sort by a **stable
key** (name, then version) so the same context always yields the same ordering.
Rendering is **per harness**, reusing the AR 570 tool-format translation: the
embedded termite harness gets in-process blocks; foreign agents get blocks over
the ACP proxy (R 695) and any referenced MCP servers over MCP-over-ACP (R 696).
The proxy calls this seam on the request path; it is never on the compositor
event loop.

## Policy

Injection is opt-in by policy with a **per-harness transparent-passthrough vs
managed** mode (same shape as the proxy's other transforms). In passthrough,
`select_and_render` returns empty and the request is untouched. In managed mode
the selected blocks are injected. The eval hooks (AR 581) decide net value of a
given skill set; this change only provides the deterministic, bounded transform.

## Rendering & event loop

None on the compositor side. Selection/rendering happen on the proxy request
path (off the main loop). An optional panel listing registered/installed skills
reuses the existing panel rendering (no new render-plan decisions); the
registry adds no tick and no polling timeout.

## Persistence

New `skill_packages` table (name, version, body, metadata, source, content
hash) as a registry cache + version-pin layer; per-scope enablement lives in the
existing `ui_state`. SQLite `user_version` bumps to the next free version.
The DB is a cache: a registered source remains the source of truth, the table is
the resurrection/offline layer.

## Invariants

- **Cache-aware injection is mandatory.** Injected skill blocks MUST ride a
  **stable prefix ordering** with cache breakpoints placed **after** the
  injected blocks; selecting/adding a skill MUST NOT reorder the existing prompt
  prefix, or it busts upstream prompt caching (the hard AR invariant).
- **AI-additive, opt-in by policy.** Injection is gated by a per-harness
  transparent-passthrough vs managed policy; passthrough is a true no-op.
- **The AI-free shell MUST NOT hard-depend on the registry.** It builds and runs
  with the registry absent or disabled; registry/injection surfaces are inert
  (no-op, not error) when no agent or proxy is configured.
- **Selection is deterministic and core-tested** (95% gate): same
  context+policy ⇒ same ordered block set; off-loop; no polling.
