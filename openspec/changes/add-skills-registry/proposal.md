# Add skills registry & distribution

## Summary

Give the AI gateway a **versioned skills registry** and a distribution UX so a
capability — a `SKILL.md`-style package — is **registered once and injected into
the relevant agent(s) by task/context**, instead of being hand-copied into each
harness's prompt. This is the _distribution_ side of the gateway's capability
injection: the proxy (`thegn-proxy`) is the single interception point all
model traffic crosses ("configure once, every harness inherits it"), and a skill
becomes one of the capability blocks it can inject.

Four capabilities:

1. **A registry of versioned skill packages** — register / install-on-demand /
   pin a `SKILL.md`-style package (name + semver + body + metadata), modeled on
   Orca's `npx skills add` install UX.
2. **Context/task-driven selection + per-harness injection** — at request time
   the proxy selects the skills relevant to the task/context and injects them,
   **translated per harness** (in-process for the embedded termite harness; over
   the ACP proxy + MCP-over-ACP for foreign agents).
3. **Cache-aware injection** — injected skill blocks ride a **stable prefix
   ordering** with cache breakpoints **after** the injected blocks, so adding /
   selecting a skill does not bust upstream prompt caching.
4. **Opt-in by policy + graceful no-op when AI is off** — injection is a managed
   transform gated by per-harness policy (transparent-passthrough vs managed),
   and the AI-free shell builds/runs with the registry absent or disabled.

Selection/injection happen on the proxy request path, not the compositor event
loop; the AI-free shell never hard-depends on any of it.

## Impact

Roadmap items (tasks.md) this change gives concrete behavior to:

- **AR 774** — Skills registry & distribution: versioned `SKILL.md` packages,
  install-on-demand, inject by task/context; the distribution UX for 544.

Realizes / extends / relates to:

- **AR 544** — skill injection (this change realizes and extends it as the
  distribution layer).
- **AR 541** — central MCP registry (sibling registry surface; a skill may
  reference registered MCP servers).
- **AR 545** — house-tool injection (injected alongside skills via the same
  cache-aware prefix discipline).
- **AR 547** — system-prompt layering (skill blocks are layered into the
  injected-prefix region).
- **AR 570** — tool-format translation (per-harness translation reused for
  per-harness skill rendering).
- **AL 455** — MCP server (skills surface to foreign agents over MCP-over-ACP).

New capability introduced (ADDED specs): `ai-gateway`.

New DB state: a `skill_packages` table (registry cache + version pins). SQLite
`user_version` bumps to the next free version.

## Rationale

- **The proxy is already the single chokepoint** every harness's model traffic
  crosses, so injecting a registered capability there is "configure once, every
  harness inherits it" — the registry only has to exist in one place.
- **Capability injection already has two realizations** (in-process for the
  embedded termite harness; over the ACP proxy + MCP-over-ACP for foreign
  agents), so a skill is rendered through the existing per-harness translation
  (AR 570) rather than a new path per harness.
- **Cache-awareness is a hard AR invariant**: a naive inject reorders the prompt
  prefix and busts upstream caching, so the registry must inject behind a stable
  prefix with breakpoints after the injected blocks.
- **Orca's `npx skills add` is the reference install UX** — versioned packages
  installed on demand — so the distribution model is proven and familiar.

## Non-goals

- **No authoring/runtime for skill _logic_.** A skill is a declarative
  `SKILL.md`-style package (prose + metadata + references); this change
  distributes and injects it, it does not execute skill code.
- **No new injection chokepoint.** Skills reuse the existing proxy / ACP /
  MCP-over-ACP injection paths; this change does not add a second interception
  point.
- **No always-on selection model.** Context/task selection is a deterministic,
  policy-bounded match at request time, not a separate inference service.
- **No AI hard-dependency.** The registry is strictly additive: the AI-free
  shell MUST build and run with the registry absent or disabled, and registry /
  injection surfaces MUST be inert (no-op, not error) when no agent or proxy is
  configured.
