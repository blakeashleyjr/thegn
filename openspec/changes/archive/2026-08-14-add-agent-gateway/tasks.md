# Tasks — per-identity agent gateway

Phased: **①② filter + inject** (the keystone `add-operating-agents-and-skills`
waits on) land first, then **③ per-harness translation**, then **④ egress
guardrails**. Core policy-eval logic is pure and unit-tested (95% gate).

## Phase ①② — Tool filtering + cache-safe injection

### 1. Policy model (core, pure)

- [ ] 1.1 `thegn-core::gateway` — `GatewayPolicy` value + layered-TOML
      parse/merge (`[gateway.agent.<id>]`: `tools.allow`, `skills`,
      `system_prompt`, `model.route`), resolved global → repo → per-agent
      front-matter; reuse the `grants.rs` glob matcher.
- [ ] 1.2 Unit tests: layering precedence, glob allow-list, absent-policy →
      transparent pass-through default.
- [ ] 1.3 Document the `[gateway.agent.<id>]` keys in
      `config/config.toml.example`.

### 2. Identity resolution

- [ ] 2.1 Map an agent's per-worktree virtual key (+ worktree/repo) → its
      `GatewayPolicy`; no new identity concept, no schema.
- [ ] 2.2 Unit tests: known identity → its policy; unknown identity → pass-through.

### 3. Tool-set filtering (`mcp/router.rs`)

- [ ] 3.1 `filter_tools(policy, advertised) -> allowed` (pure, core) + unit tests
      (allow-list, deny-unlisted, empty policy = all).
- [ ] 3.2 Apply in `handle_tools_list` — advertise only the allowed set.
- [ ] 3.3 **Deny-unlisted** in `handle_tools_call` — refuse a call to a
      non-allowed tool with an MCP error, before any side effect.

### 4. Cache-safe injection (`proxy/transform.rs`)

- [ ] 4.1 `plan_injection(policy, request) -> InjectionPlan {system_blocks,
tool_blocks}` (pure, core).
- [ ] 4.2 Cache-safe splice helper: deterministic fixed block order, cache
      breakpoint placed **at/after** the last injected block, idempotent (marker-
      guarded).
- [ ] 4.3 Unit tests — **the cache invariant**: for a fixed policy+request the
      serialized cacheable prefix is byte-identical across two turns, and the
      breakpoint index `>=` the last injected-block index; double-apply is a no-op.
- [ ] 4.4 Wire identity→policy→plan into the proxy relay request path (I/O seam,
      no policy logic).

## Phase ③ — Per-harness tool-format translation

- [ ] 5.1 Canonical tool/skill definition → per-harness tool schema table,
      co-located with the proxy provider registry.
- [ ] 5.2 Translate injected/filtered tool blocks per resolved harness as the last
      step before egress; filtering/injection stay harness-agnostic.
- [ ] 5.3 Unit tests: one canonical def → Claude Code / Codex / OpenCode shapes;
      round-trips the allowed set unchanged in count.

## Phase ④ — Egress guardrail chain

- [ ] 6.1 Pure guardrail matchers (core): regex filter, secret-scan (AR 574),
      PII/redaction (AR 575); ordered chain regex → secret → moderation.
- [ ] 6.2 Unit tests: secret/PII hit redacts or blocks per policy mode; clean
      prompt passes untouched; redaction runs before breakpoint placement (does
      not perturb the cached prefix).
- [ ] 6.3 Wire the chain onto the proxy egress path (I/O seam); off by default
      per-policy.

## Spec + validation

- [ ] 7.1 Add `specs/agent-gateway/spec.md`; apply MODIFIED deltas to
      `mcp-servers` (filtered list + deny-unlisted call) and `llm-proxy`
      (per-identity injection + egress chain).
- [ ] 7.2 `smoke.sh` coverage for the pass-through default (no policy → agent sees
      the full house-tool set, nothing injected).
- [ ] 7.3 Run `just ci` (fmt-check + lint + build + test + coverage + smoke +
      openspec-validate + nix-build).
