# Add the per-identity agent gateway (tool-filtering + injection chokepoint)

## Summary

superzej already routes foreign agents through the proxy — `route_agent` mints a
**per-worktree virtual key** and relays the traffic — but there is no single
place where that identity is turned into _policy_. The pieces exist and are
scattered: `bouncer.rs` gates individual shell/edit/write calls for **human**
approval (not role-based filtering); `mcp/router.rs` advertises house tools
(conditionally per provider, but to everyone); `grants.rs` glob-checks
capability acquisition. None of them answer "given _this_ agent identity, which
tools may it see, what skills/prompt get injected, and what must be scrubbed on
the way out."

This change builds that missing **chokepoint** — the per-identity gateway — as
the latent keystone the agent-layer roadmap already leans on. It is a single
seam, phased, that does four things keyed off the agent's virtual-key identity:

1. **FILTER** the tool set an agent may see and call — at `mcp/router.rs`
   `tools/list` (advertise only the policy's allowed set) and **deny-unlisted at
   `tools/call`** (a call to a tool the policy didn't allow is refused before any
   side effect).
2. **INJECT** skills + a system-prompt layer per agent on the proxy request path
   (`proxy/transform.rs`), **prompt-cache-safe** — stable prefix ordering with
   the cache breakpoint placed _after_ the injected blocks so injection never
   busts the upstream prompt cache (the same discipline `proxy/compress.rs`
   holds for compressed tool output).
3. **TRANSLATE** injected tool/skill definitions to each harness's tool format
   (co-located with the provider registry) so one definition works across Claude
   Code / Codex / OpenCode.
4. **GUARDRAIL** an egress chain (regex filter → secret-scan → moderation) that
   inspects outbound prompts at the one place all AI traffic crosses.

The policy itself is **declarative layered TOML** front-matter (agent identity →
`{allowed tools, injected skills, injected system-prompt, model routing}`),
consistent with `config_enum!` and the markdown+TOML agent definitions the
content change assumes. No new DSL/CEL dependency. The pure
identity+request→`{allowed tools, injected blocks}` evaluation lives in
**superzej-core** (coverage-gated); the wiring into the proxy/MCP I/O path is
the host/proxy crate.

## Impact

- tasks.md: **AR 541** (central MCP registry — advertise/translate per harness),
  **AR 545–547** (house-tool injection, tool filtering/override, system-prompt
  layering), **AR 570** (tool-format translation — the "one MCP server, every
  harness" enabler), **AR 573–575** (prompt-injection scanning, secret detection,
  PII/redaction on egress — the opsec guardrail chain), and **R 695** (AR gateway
  as an ACP proxy so injection/filtering reach any ACP agent). This change is the
  **seam**; it does not itself ship the agent/skill _content_.
- **Unblocks `add-operating-agents-and-skills`** — that change explicitly scopes
  itself to the agent + skill _definitions_ and their tool-set _restriction_ and
  states it "depends on that seam existing" (its Non-goals: "Building the
  proxy/gateway injection mechanism itself — that is AR 541–551 / R 695"). This
  change is exactly that mechanism. `add-agent-steerable-review` (AR 570) and
  `add-agent-autoregistration` likewise assume the filter/inject chokepoint.
- **Capabilities** — ADD a new `agent-gateway` capability (the per-identity
  policy chokepoint). MODIFY `mcp-servers` (tools/list filtered + deny-unlisted
  at tools/call, keyed by identity) and `llm-proxy` (per-identity injection on
  the request path + egress guardrail chain).
- **superzej-core** — a new pure `gateway` module (policy load/layer + evaluate)
  under coverage; `proxy/transform.rs` gains a cache-safe injection helper.
- **superzej-proxy / superzej-host** — wire the evaluated policy into the relay
  request path and the MCP router; the identity is the existing per-worktree
  virtual key (no new key concept).
- **No DB schema change** — policy is declarative files resolved like layered
  config; no `user_version` bump.
- **No new event-loop wake path and no render damage** — the gateway lives
  entirely on the off-loop proxy/ACP path; the compositor loop, its wake
  sources, and all three damage channels (`full`/`chrome`/`dirty_panes`) are
  untouched.
- **AI-free shell unaffected** — filtering/injection are purely additive on the
  AI path; with no agent connected the shell behaves exactly as today.

## Rationale

The design north-star is **agentgateway** (the open-source agent-traffic gateway):
one identity-keyed chokepoint that filters tools, injects capabilities, and
guards egress for every agent behind it. superzej wants that _shape_, but **not**
its substrate — we do **not** vendor a Kubernetes controller/daemon or adopt a CEL
policy engine. superzej already owns the choke points agentgateway externalizes:
the proxy request path (`proxy/transform.rs`), the MCP router (`mcp/router.rs`),
and a per-worktree virtual-key identity minted by `route_agent`. Building the
gateway **natively** over those seams, with policy expressed as the same layered
TOML the rest of superzej uses, keeps the AI-free shell independent and the whole
thing hermetic — no new runtime, no new DSL. The single hard correctness
constraint is prompt-cache safety: injection reorders/adds prompt blocks, and if
the cache breakpoint lands before an injected block the upstream cache is busted
and cost balloons — so the injection ordering is a first-class, tested invariant
mirroring `compress.rs`'s determinism contract.

## Non-goals

- **A CEL / new policy DSL** — policy is declarative layered TOML front-matter;
  no new dependency, no expression language now.
- **A skill / agent marketplace or remote policy registry** — policies and
  definitions are local files (cf. the deferred plugin marketplace).
- **A Kubernetes controller / standalone gateway daemon** — this is the native
  in-proxy chokepoint, not a vendored agentgateway deployment.
- **The agent + skill _content_** — role-scoped agent definitions and skill
  workflows are `add-operating-agents-and-skills`; this change builds the seam
  they ride, and unblocks it.
- **Any AI-free-shell dependency** — the gateway is strictly the AI layer; the
  shell never hard-depends on it.
- **Deep model routing / cost tiering (AR 560–565)** — the policy may _name_ a
  model route, but the routing engine itself is out of scope here.
