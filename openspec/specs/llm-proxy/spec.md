# LLM Proxy

## Purpose

The LLM proxy (`szproxy`) is the lower control plane for AI traffic: a
dual-protocol relay that fronts upstream model providers with failover, circuit
breaking, streaming passthrough, and per-agent virtual keys with budgets. It is
strictly additive — the AI-free shell never depends on it — and runs as a managed
daemon/pinned program.

## Requirements

### Requirement: Dual-protocol streaming relay

The proxy SHALL relay both the Anthropic and OpenAI protocols (translating SSE) and MUST stream responses through without buffering.

#### Scenario: Streaming passthrough

- **WHEN** an upstream streams a response
- **THEN** the proxy forwards chunks without buffering the full response

### Requirement: Failover and per-upstream circuit breaking

The proxy SHALL provide ordered sequential failover across upstreams and MUST track limit exhaustion / `Retry-After` per upstream with a cooldown circuit breaker before failing back.

#### Scenario: Upstream exhausted

- **WHEN** an upstream signals limit exhaustion
- **THEN** the proxy fails over to the next upstream and places the exhausted one
  in cooldown

### Requirement: Per-agent virtual keys with budgets

The proxy SHALL resolve per-agent virtual-key identities and MUST attribute spend and enforce per-identity budgets.

#### Scenario: Per-identity budget

- **WHEN** requests arrive under a virtual key with a budget
- **THEN** spend is attributed to that identity and the budget is enforced

### Requirement: Proxy is additive and runs as a managed daemon

The proxy SHALL run as a host-managed daemon/pinned program, and the AI-free shell MUST function fully when the proxy is absent.

#### Scenario: Shell without the proxy

- **WHEN** no proxy is configured
- **THEN** the shell operates normally with AI features simply unavailable
