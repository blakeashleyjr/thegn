# Add operating agents (role-scoped, tool-restricted) + skills

## Summary

Introduce two declarative primitives to the agent layer, borrowed from Forge:

1. **Operating agents** — named, role-scoped agent definitions with a **restricted
   tool set** and a system prompt, declared in markdown + TOML/YAML front-matter.
   Ship three defaults mirroring Forge's model — an **executor** (read/write/patch/
   shell), a **read-only researcher** (no mutation), and a **planner** (writes only
   plans) — and let users define their own under the repo or global config dir.
2. **Skills** — reusable, named workflows _orthogonal_ to agents (e.g.
   "resolve-conflicts", "write-a-plan"), injected by the proxy/gateway so **every**
   harness can invoke them, not just the first-party agent.

The tool restriction is enforced at the proxy/ACP tool-filtering seam (an agent's
declared tool set bounds what it may call), composing with — not replacing — the
sandbox policy engine and bouncer.

## Impact

- **AR 541–551** — capability injection / prompt layering / tool filtering: skills
  are injected and agents' tool sets are filtered at the gateway, "configure once,
  every harness inherits it".
- **AR 570** — tool-format translation: an injected skill/tool is translated per
  harness so one definition works across Claude Code / Codex / OpenCode.
- **R 695** (ACP proxy) — the tool-filtering / injection applies to any ACP agent,
  not only the embedded first-party one.
- **Q (termite-agent roadmap)** — role-scoped operating agents are a clean
  structure for the first-party harness's built-in modes (Q/S/T track against its
  ROADMAP).

Extends the `agent` capability. **No DB schema change** — agents and skills are
declarative files resolved like layered config.

## Rationale

Forge's `forge`/`sage`/`muse` trio shows the value of _role-scoped, tool-restricted
operating agents_: a research agent that literally cannot modify files is safer
and more predictable than one general agent with all tools. Separately, Forge's
**skills** decouple "what can be done" (a reusable workflow) from "how to decide
when to do it" (agent reasoning), and its markdown-front-matter agent definitions
make customization a low barrier. superzej already has the injection/filtering
chokepoint designed (AR "configure once, every harness inherits it", #570
tool-format translation, R 695 ACP proxy) — this change is the _content_ that
flows through it: declarative agents + skills. cmux-skills' `npx skills add`
distribution is a later idea for sharing skills.

## Non-goals

- **Building the proxy/gateway injection mechanism itself** — that is AR 541–551 /
  R 695; this change defines the _agent + skill definitions_ and the tool-set
  _restriction_ that ride it, and depends on that seam existing.
- **A marketplace / remote skill registry** — parked until there are users (cf.
  the deferred plugin marketplace); definitions are local files for now.
- **Prompt-caching correctness of injection** — the known cache-vs-injection
  tension (stable prefix ordering, breakpoints after injected blocks) is AR's
  concern and is called out there; skills defined here must be injectable in a
  cache-aware way.
- **AI-free shell dependency** — agents/skills are purely the AI layer; the shell
  never depends on them.
