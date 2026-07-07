# LLM Proxy

## ADDED Requirements

### Requirement: The proxy injects per-identity gateway context cache-safely

The proxy SHALL apply the resolved gateway policy for the request's virtual-key
identity on the request path, injecting the policy's skills and system-prompt layer
into the outbound request. Injection MUST be prompt-cache-safe: injected blocks are
assembled in a fixed deterministic order so the cacheable prefix is byte-identical
turn-over-turn, and the prompt-cache breakpoint MUST be placed at or after the last
injected block so injected content never precedes the cached prefix. Applying the
plan MUST be idempotent. When no policy applies, the request path is unchanged.

#### Scenario: Injection holds the upstream cache

- **WHEN** a policy injects into two successive turns of the same conversation
- **THEN** the cacheable prefix up to and including the breakpoint is byte-identical
  across both turns, so the upstream prompt cache is preserved

#### Scenario: No policy, unchanged request

- **WHEN** a request's identity has no gateway policy
- **THEN** the proxy relays the request without injecting or reordering blocks

### Requirement: The proxy runs an egress guardrail chain per policy

The proxy SHALL run the gateway's ordered egress guardrail chain — regex filter →
secret-scan → moderation — on outbound prompts when the identity's policy enables
it, redacting or blocking per the policy mode before the prompt leaves the box. The
chain MUST be off by default, MUST run on a stage that does not perturb the cached
prefix, and its matchers MUST be pure/unit-tested in core.

#### Scenario: A secret is blocked before egress

- **WHEN** an outbound prompt contains a value the secret-scan matches under a
  policy that enables the chain
- **THEN** the prompt is blocked or the secret redacted per policy before it leaves

#### Scenario: Chain disabled relays unchanged

- **WHEN** the policy does not enable the guardrail chain
- **THEN** the outbound prompt is relayed unchanged
