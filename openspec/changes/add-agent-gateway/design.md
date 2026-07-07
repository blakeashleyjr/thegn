# Design — per-identity agent gateway

## Identity: the per-worktree virtual key

The gateway keys every decision off **agent identity**, and superzej already has
one: `route_agent` wires a foreign agent through the proxy under a **per-worktree
virtual key**. That key (plus the worktree/repo it belongs to) _is_ the identity.
No new identity concept, no schema: the relay already carries it on the request,
and the MCP router already knows the worktree it was constructed `with_git`/
`with_forge` for. The gateway resolves that identity → a `GatewayPolicy` and
applies it at the two I/O seams below.

## The policy (declarative layered TOML)

Policy is a declarative document — layered like the rest of superzej config
(global → repo `.superzej.*` → per-agent front-matter), resolved into a pure
`GatewayPolicy` value:

```toml
[gateway.agent.researcher]
tools.allow   = ["read_*", "git_log", "sem_*"]   # globs, deny-unlisted
skills        = ["write-a-plan"]                  # injected skill ids
system_prompt = "house-rules"                     # injected prompt-layer id
model.route   = "cheap"                            # advisory route hint (AR 560, out of scope to honor here)
```

Globs reuse the `grants.rs` matcher discipline (`*` within a segment, `**`
across). Absent a policy for an identity, the gateway is a **transparent
pass-through** — every tool visible, nothing injected, no guardrail beyond the
existing ones — so the current behavior is the zero-policy default.

The pure core module (`superzej-core::gateway`) owns: parse+layer the TOML,
build `GatewayPolicy`, and the two pure evaluators —
`filter_tools(policy, advertised) -> allowed` and
`plan_injection(policy, request) -> InjectionPlan {system_blocks, tool_blocks}`.
No tokio, no termwiz, no I/O — coverage-gated at 95%.

## The four gateway functions and where each hooks

### ① Tool-set filtering — `mcp/router.rs`

- `tools/list` (`handle_tools_list`): after the router assembles the advertised
  house-tool set, `filter_tools(policy, advertised)` drops everything the policy
  doesn't allow. An agent literally cannot _see_ a tool outside its set.
- `tools/call` (`handle_tools_call`): **deny-unlisted** — before dispatch, the
  called tool name is re-checked against the same allowed set; a call to a
  non-allowed tool is refused with an MCP error and no side effect. Filtering the
  list is not enough — a hand-crafted call must also be rejected at the door.

This is enforcement in **core**, so it holds identically for the in-process MCP
router and (via R 695) the MCP-over-ACP path.

### ② Skill + system-prompt injection — `proxy/transform.rs`

On the request path, `plan_injection` produces the blocks to add: skill
definitions and a system-prompt layer (house rules / repo context). A new
cache-safe helper splices them into the request body. This is where the hard
invariant lives — see below.

### ③ Per-harness tool-format translation — co-located with the provider registry

An injected tool/skill has one canonical definition; each harness (Claude Code /
Codex / OpenCode) wants it in its own tool-call schema. The translation table is
co-located with the proxy provider registry (`add-proxy-provider-registry`)
because that is already where per-provider request shape is known. `filter_tools`
runs on the canonical set; translation is the last step before the body leaves,
so filtering/injection stay harness-agnostic and only the wire shape varies.

### ④ Egress guardrail chain — proxy request path

A conservative, ordered chain inspects the **outbound** prompt at the single
chokepoint all AI traffic crosses: **regex filter → secret-scan → moderation**.
Secret-scan (AR 574) blocks an API key/credential from leaving the box;
PII/redaction (AR 575) scrubs before egress; prompt-injection scanning of tool
_results_ (AR 573) is a sibling on the ingress side. The chain is off by default
per-policy; a hit either redacts, blocks, or annotates per the policy's mode.
Ordering matters: redaction runs **before** injection-cache-breakpoint placement
so a redaction never rewrites a cached prefix (see below).

## The cache-safe injection ordering invariant (first-class)

Prompt caching keys on a **byte-stable prefix**; the upstream caches everything
up to a **cache breakpoint** and re-bills only the suffix. Injection _adds_ and
_orders_ prompt blocks, so a careless splice moves the breakpoint's contents and
busts the cache — turning a cheap cached turn into a full re-bill. Same class of
bug `compress.rs` avoids by being pure/deterministic/idempotent.

The gateway's contract, enforced and tested:

1. **Stable prefix ordering.** Injected blocks are assembled in a **fixed,
   deterministic order** (system-prompt layer, then skills, then the original
   request's system/tools) from the policy — identical policy ⇒ byte-identical
   injected prefix turn-over-turn.
2. **Breakpoint after injected blocks.** The prompt-cache breakpoint
   (`cache_control`) is placed **at or after** the last injected block, never
   before it. Injected content sits inside the cached prefix and is stable, so
   the cache _hit_ is preserved: the injection is amortized into the cached
   prefix rather than re-billed each turn.
3. **Injection is idempotent + order-independent of egress redaction.** Applying
   the plan twice is a no-op (guarded by a marker), and egress redaction runs on
   a stage that does not perturb the cached prefix.

Design consequence: `plan_injection` is pure and deterministic; the splice
helper's _only_ nondeterminism-free job is to keep the breakpoint trailing the
injected blocks. A unit test asserts that for a fixed policy + request, the
serialized cacheable prefix (everything up to and including the breakpoint) is
byte-identical across two turns, and that the breakpoint index is `>=` the last
injected-block index.

## Pure-core vs proxy/host I/O split

- **superzej-core** (`gateway` module + a `proxy/transform.rs` helper): policy
  parse/layer, `GatewayPolicy`, `filter_tools`, `plan_injection`, the cache-safe
  splice, and the guardrail-chain _matchers_ (regex/secret/PII detectors as pure
  functions). All unit-tested, 95% gate.
- **superzej-proxy / superzej-host** (I/O wiring): resolve identity→policy from
  the request's virtual key, call the pure evaluators, apply the plan to the live
  body, run the guardrail chain on egress, and enforce deny-unlisted in the MCP
  router. No policy _logic_ here — just the seam.

## SQLite / schema

**No schema change and no `user_version` bump.** Policy is declarative files
resolved like layered config; nothing is persisted in the DB. If a later change
wants per-session policy audit rows that is its own migration — out of scope
here. (Stated per the config.yaml design rule.)

## Event loop & render damage

**None.** The gateway lives entirely on the off-loop proxy/ACP request path
(already off the compositor loop, like all agent I/O). It adds **no new loop wake
source** and touches **no render damage channel** (`full`/`chrome`/`dirty_panes`)
— an injected prompt or a filtered tool list never repaints the shell. The
0%-idle contract and the `render_plan::plan()` invariants are unaffected.
