# Placement

## ADDED Requirements

### Requirement: Spillover is the last paid lane, ordered, clamped, and health-tracked

superzej SHALL burst to an external sandbox vendor only after the owned pool
and autoscale lanes are exhausted, walking `spillover_provider_order`
filtered to the effective `allowed_providers` ceiling. Provider health SHALL
mirror the proxy's exhaustion pattern: a payment failure marks the provider
budget-dead until the ledger says otherwise (surviving restarts), a quota
rejection cools it down honoring Retry-After, a create failure cools down
with capped escalation — and every marker fails back automatically on
expiry, in original preference order.

#### Scenario: Budget-dead provider is skipped, then fails back

- **WHEN** Sprites returns 402 on a spillover create and Daytona is next in
  order
- **THEN** the sandbox lands on Daytona, subsequent spawns skip Sprites even
  after a restart, and Sprites is retried only once its budget verdict
  clears

#### Scenario: Ready spares beat paid creates

- **WHEN** the owned pool is exhausted and a ready pool spare exists on a
  quota-cooled spillover provider
- **THEN** the spare is claimed (a start, not a create) before any autoscale
  or spillover create is attempted

### Requirement: Compute spend is capped by its own ledger with fixed and metered categories

superzej SHALL track placement cost in a compute ledger separate from the
LLM proxy's but identical in shape (scope → zone → global caps, monthly
window, kill-switch): managed hosts accrue fixed hourly cost from create to
destroy via idempotent watermark accrual; spillover accrues per-create plus
active-time metered cost. Budget verdicts MUST gate only the lanes that add
spend — a cap breach refuses or defers paid creates while packed placement
onto already-paid hosts keeps serving.

#### Scenario: Cap breach stops paid lanes, not packing

- **WHEN** the monthly compute cap is reached mid-period
- **THEN** autoscale and spillover creates are refused (or queued per
  `on_exhaustion`) while new sandboxes still pack onto existing hosts

#### Scenario: Accrual is idempotent across gaps

- **WHEN** superzej is closed for a week while a managed host lives
- **THEN** the next accrual tick adds exactly the gap's cost, once

### Requirement: Presets set preferences, never constraints

superzej SHALL expand a named placement preset at the config layer that set
it, beneath that layer's explicit keys, and a preset MUST be structurally
unable to touch constraint keys — every preset expansion still flows through
zone/repo clamps and the mode floor.

#### Scenario: Explicit key beats its own layer's preset

- **WHEN** one file sets `preset = "cost_optimized"` and
  `pack_strategy = "spread"`
- **THEN** the effective strategy is spread, with the preset supplying the
  remaining preferences

### Requirement: Repo overlays cannot loosen fleet placement policy

superzej SHALL treat repo `.superzej.*` placement keys as a clamped request:
preferences apply, `allowed_providers` may only narrow, the spillover order
is filtered to the allow-list with surfaced clamp events, and every
fleet-scoped constraint (overcommit, host/spend ceilings, autoscale lanes,
prices) is denied loudly — a checked-in file must never alter packing
economics for other repos' tenants on shared hosts.

#### Scenario: Hostile repo overlay is clamped, not obeyed

- **WHEN** a cloned repo's `.superzej.toml` sets `overcommit = 16.0` and
  `spillover_provider_order = ["evil-provider"]`
- **THEN** the overcommit is denied with a visible clamp event and the order
  entry is dropped as outside the allow-list; the worktree still opens
