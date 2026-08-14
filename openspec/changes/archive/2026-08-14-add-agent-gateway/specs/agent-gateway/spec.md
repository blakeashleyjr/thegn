# Agent Gateway

## ADDED Requirements

### Requirement: A per-identity policy governs each agent's tools and injected context

thegn SHALL resolve every agent's identity — its per-worktree virtual key (plus
the worktree/repo it belongs to) — to a declarative `GatewayPolicy` describing its
allowed tool set, injected skills, injected system-prompt layer, and model route.
The policy MUST be expressed as layered TOML (`[gateway.agent.<id>]`), resolved
global → repo overlay → per-agent front-matter with the more-specific layer
winning, and the evaluation of identity + request → `{allowed tools, injected
blocks}` MUST be a pure, unit-tested function in core (no I/O). When no policy
matches an identity, the gateway MUST behave as a transparent pass-through.

#### Scenario: A configured identity resolves to its policy

- **WHEN** an agent whose virtual key maps to `[gateway.agent.researcher]` issues a request
- **THEN** the gateway resolves the `researcher` policy and applies its allowed
  tool set, injected skills, and system-prompt layer to that agent

#### Scenario: Layering resolves the more-specific policy

- **WHEN** a repo overlay and a per-agent front-matter both set fields for the same identity
- **THEN** the resolved policy takes the more-specific layer's value for each field

### Requirement: The gateway filters an agent's tool set by identity and denies unlisted calls

The gateway SHALL restrict the tools an agent may both see and invoke to the set
its policy allows. At `tools/list` it MUST advertise only the allowed tools; at
`tools/call` it MUST refuse a call to any tool outside the allowed set with an
error and without producing a side effect (deny-unlisted). Allowed-tool matching
MUST support glob scopes (`*` within a segment, `**` across segments), matching the
capability-grant glob discipline, and MUST be pure/unit-tested.

#### Scenario: Only allowed tools are advertised

- **WHEN** an agent whose policy allows `read_*` and `git_log` requests `tools/list`
- **THEN** only the house tools matching that allow-list are advertised, and
  disallowed tools are absent from the response

#### Scenario: A call to an unlisted tool is denied before any side effect

- **WHEN** the same agent issues `tools/call` for a tool outside its allowed set
- **THEN** the call is refused with an error and no side effect (file write, shell
  exec, ref advance) occurs

#### Scenario: No policy means the full house-tool set

- **WHEN** an agent has no matching gateway policy
- **THEN** the full advertised house-tool set is visible and callable (transparent
  pass-through)

### Requirement: The gateway injects skills and a system-prompt layer per agent

On the proxy request path the gateway SHALL inject the policy's skills and
system-prompt layer into the outbound request so every harness inherits them,
without the agent having to declare them. The set of injected blocks MUST be
derived by a pure `plan_injection(policy, request)` function in core, and applying
the plan MUST be idempotent (re-applying it to an already-injected request is a
no-op).

#### Scenario: A policy's skills and prompt layer are injected

- **WHEN** a request from an agent whose policy names skills `["write-a-plan"]` and
  system prompt `"house-rules"` is relayed
- **THEN** the outbound request carries those skill definitions and the house-rules
  system-prompt layer

#### Scenario: Injection is idempotent

- **WHEN** the injection plan is applied to a request that already contains the
  injected blocks
- **THEN** no duplicate blocks are added and the request is unchanged

### Requirement: Injection MUST be prompt-cache-safe

The gateway MUST preserve the upstream prompt cache when injecting. Injected blocks
MUST be assembled in a fixed, deterministic order so the cacheable prefix is
byte-identical turn-over-turn for a fixed policy and request, and the prompt-cache
breakpoint (`cache_control`) MUST be placed at or after the last injected block so
injected content lives inside — never ahead of — the cached prefix. Egress
redaction MUST run on a stage that does not perturb the cached prefix.

#### Scenario: Injection preserves the upstream cache prefix

- **WHEN** the same policy injects into two successive turns of a conversation
- **THEN** the serialized cacheable prefix (everything up to and including the cache
  breakpoint) is byte-identical across both turns, and the breakpoint falls at or
  after the last injected block

#### Scenario: A breakpoint is never placed before an injected block

- **WHEN** the gateway splices injected system/skill blocks into a request that
  carries a cache breakpoint
- **THEN** the resulting breakpoint index is greater than or equal to the index of
  the last injected block

### Requirement: Injected tools/skills are translated per harness

The gateway SHALL translate each injected or filtered tool/skill definition from
its single canonical form into the tool-call format of the target harness (e.g.
Claude Code, Codex, OpenCode) as the last step before the request leaves, so one
definition works across harnesses. Filtering and injection MUST operate on the
canonical set and remain harness-agnostic; only the on-the-wire shape varies by
harness.

#### Scenario: One definition, per-harness shapes

- **WHEN** a canonical tool definition is injected for two agents on different
  harnesses
- **THEN** each agent receives the tool in its own harness's tool-call format while
  the allowed set's membership is identical

### Requirement: An egress guardrail chain scans outbound prompts

The gateway SHALL run an ordered guardrail chain — regex filter → secret-scan →
moderation — on outbound prompts at the single proxy chokepoint all AI traffic
crosses. Secret-scan MUST be able to block a detected credential/API key from
leaving the box and PII redaction MUST be able to scrub before egress, per the
policy's mode (redact / block / annotate). The chain MUST be off by default and
enabled per policy, and its matchers MUST be pure/unit-tested in core.

#### Scenario: A detected secret is blocked or redacted on egress

- **WHEN** an outbound prompt contains a value the secret-scan matches and the
  policy enables the chain
- **THEN** the prompt is blocked or the secret redacted per the policy mode before
  it leaves the box

#### Scenario: A clean prompt passes untouched

- **WHEN** an outbound prompt matches no guardrail and/or the chain is disabled by
  policy
- **THEN** the prompt is relayed unchanged

### Requirement: The gateway is additive and off the compositor loop

The gateway SHALL operate entirely on the off-loop proxy/ACP path and MUST NOT add
a compositor event-loop wake source or touch any render damage channel. With no
agent connected the AI-free shell MUST behave exactly as it does without the
gateway; filtering, injection, translation, and guardrails are strictly additive
to the AI path.

#### Scenario: No agent, no effect

- **WHEN** the shell runs with no agent connected
- **THEN** the gateway performs no work, the loop's idle and render invariants are
  unchanged, and shell behavior is identical to a build without the gateway
