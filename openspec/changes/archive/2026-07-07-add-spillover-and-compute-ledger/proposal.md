# Add vendor spillover, the compute spend ledger, and placement presets

## Summary

Complete the placement engine's economics. **Spillover**: when the owned pool
(packed + dedicated + autoscale) is exhausted, burst to an external sandbox
vendor — Sprites or Daytona, through the provider adapters that already exist
— chosen from an ordered, allow-list-clamped preference, with proxy-style
exhaustion tracking (a 402 marks the provider budget-dead across restarts; a
429 honors Retry-After; a create failure cools down with escalation) and
automatic fail-back on expiry. **Compute ledger**: a spend ledger for
placement, deliberately separate from the LLM proxy's but byte-similar in
shape (scope → zone → global caps, monthly window, kill-switch), with two cost
categories that make "squeeze owned capacity before paying vendor markup"
expressible — _fixed_ (managed hosts: hourly rate × lifetime, watermark
accrual) and _metered_ (spillover: per-create + active-time). Budget verdicts
gate the paid lanes (autoscale, spillover): `Allow` proceeds, `Refuse` skips
the lane, and the compute analog of the proxy's Downgrade is _deferral_ —
compute can't swap a cheaper VM mid-spawn. **Presets**: four named bundles
(`cost_optimized`, `latency_optimized`, `isolated`, `balanced`) expanding to
preference keys only — a code table, individually overridable, never able to
widen a constraint. Plus the trust-completion of the config surface:
zone/repo placement clamps (`ZonePlacement`, `classify_repo_placement` with
the hostile-repo suite) and `placement budget` / `explain` CLI surfaces.

## Impact

- **Config** — `[placement]` gains `preset`, `allowed_providers` (ceiling),
  `spillover_provider_order` (preference filtered to the allow-list),
  `max_monthly_spend`, `[placement.price.<provider[:size]>]` rate table;
  `[zone.<name>.placement]` clamp block; repo `.thegn.*` gains a clamped
  `[placement]` preference subset (everything fleet-scoped is Forbidden with
  loud ClampEvents).
- **DB** — v36: `compute_budgets` (mirror of `proxy_budgets` minus tokens),
  `compute_meters` (watermark accrual rows); `store/compute.rs`
  `ComputeLedgerStore`.
- **Core** — `spillover.rs` (classify/cooldown/pick/spare-claimable + the
  `ComputeBudget` seam), `compute_spend.rs` (verdict ladder, triple
  attribution, pure accrual math), preset expansion in `config_placement.rs`.
- **svc** — provider create errors carry HTTP status + Retry-After context
  (mechanical; mock fixtures for 402/429).
- **Flows** — the broker's spillover lane (prefer claiming a ready pool spare
  over any paid create; sticky sessions — fail-back affects new placements
  only), meter start/stop wiring (VPS create/destroy, spillover claim/close),
  a 60s accrual tick beside the reaper, `sync_compute_budget_caps` at
  startup/CLI.
- **CLI** — `placement budget` (set-limit / kill / unkill), richer
  `placement explain` (budget verdicts in traces), `config explain
placement.*` clamp traces.
- **tasks.md**: groups U/V (proxy budget precedent), AE, 749, 244.

## Rationale

The engine so far chooses among machines thegn pays for regardless of use.
The billing research this design rests on says vendors charge a 5–15×
per-vCPU markup but bill only active time — so spillover is the correct _last_
lane (after packing onto sunk-cost hosts and before refusing work), and it
must be budget-gated to be safe to automate. A separate ledger (the user's
call) keeps model spend and compute spend independently capped while the
shapes stay identical, so the mental model transfers.

## Non-goals

- **New vendor adapters** — Sprites + Daytona only (the adapters exist;
  spillover is a broker lane, not an integration project).
- **Extending the LLM proxy ledger** — explicitly a separate ledger.
- **Migrating spilled sessions back** — sticky; recorded as future work.
- **Adaptive overcommit under budget pressure** — v1 is static ceilings +
  verdicts; the lane order already encodes squeeze-before-spill.
