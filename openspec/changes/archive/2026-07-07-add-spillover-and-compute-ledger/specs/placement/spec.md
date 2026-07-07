# Placement

## ADDED Requirements

### Requirement: Spillover is the last paid lane, ordered and health-tracked

superzej SHALL burst to an external sandbox vendor only after the owned pool
and autoscale lanes are exhausted, walking the ordered `spillover_envs` list
(provider-placement envs riding the existing provider pipeline; the list is
global/profile config only — a repo overlay structurally cannot name
spillover targets). Provider health SHALL mirror the proxy's exhaustion
pattern: a payment failure parks the provider until the ledger clears it
(markers survive restarts), a quota rejection cools it down, a create
failure cools down with capped escalation — and every marker fails back
automatically on expiry, in original preference order. A spilled worktree is
sticky: fail-back affects new placements only.

#### Scenario: Budget-dead provider is skipped, then fails back

- **WHEN** the sprites spillover env fails a create with a payment error and
  a daytona env is next in order
- **THEN** the next spill lands on daytona, subsequent spawns skip sprites
  even after a restart, and sprites is retried only once its marker/ledger
  clears

#### Scenario: Quota cooling gates creates, not spare starts

- **WHEN** a spillover provider is quota-cooled while holding a ready pool
  spare
- **THEN** the spare remains claimable (a start, not a create) while new
  creates on that provider wait out the cooldown

### Requirement: Compute spend is capped by its own ledger with fixed and metered categories

superzej SHALL track placement cost in a compute ledger separate from the
LLM proxy's but identical in shape (scope → zone → global caps, monthly
window, kill-switch): managed hosts accrue fixed hourly cost from create to
destroy via idempotent watermark accrual (vendor-metered spillover accrual is
recorded future work pending provider usage APIs). Budget verdicts MUST gate only the lanes that add
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

superzej SHALL expand a named placement preset into only the preference keys
still at their built-in defaults — an explicitly-set key always wins — and a
preset MUST be structurally unable to touch constraint keys; every expansion
still flows through the zone floor and mode clamp.

#### Scenario: Explicit key beats the preset

- **WHEN** config sets `preset = "cost_optimized"` and
  `pack_strategy = "spread"`
- **THEN** the effective strategy is spread, with the preset supplying the
  remaining preferences (packed mode, queue on exhaustion)

### Requirement: Repo overlays cannot touch fleet placement policy

superzej SHALL keep every `[placement]` key out of the repo overlay schema
entirely (the `[host.*]` structural-exclusion precedent): a checked-in
`.superzej.*` file cannot set overcommit, spend caps, autoscale lanes,
prices, or spillover targets, because the overlay simply has no such table —
packing economics for shared hosts are never authorable from the
least-trusted layer.

#### Scenario: Hostile repo overlay cannot reach placement config

- **WHEN** a cloned repo's `.superzej.toml` smuggles a `[placement]` table
  with `overcommit = 16.0` and its own spillover list
- **THEN** none of it reaches the resolved config (the overlay schema has no
  placement table) and the worktree still opens
