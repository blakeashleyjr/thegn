# Design

## Spillover is a broker lane, not an adapter layer

The output of the spillover lane is the EXISTING provider path
(`Placement::Provider` + `provision_provider_env_named` + the warm pool) —
Sprites and Daytona adapters are reused verbatim. What is new is choice and
health: `pick_spillover(order ∩ allowed, markers, budget_ok, now)` in pure
core, with markers persisted in the v34 `placement_health` table under
`provider:<name>` keys (one cooldown table serves autoscale lanes and
spillover providers). Classification mirrors the proxy's vocabulary, reusing
the pure `proxy::backoff` machinery: HTTP 402 ⇒ `Budget` (not time-cooled —
budget death is a live ledger predicate and survives restarts), 429 ⇒ `Quota`
(Retry-After honored), timeout/5xx ⇒ `CreateFailure` (escalating cooldown,
capped). Fail-back is implicit (`now ≥ retry_at`), success clears the marker.

Pool interplay: a READY pool spare on a spillover provider is preferred over
ANY paid create (autoscale included) — it is already-paid provisioning.
`spare_claimable` draws the line: a Quota-marked provider may still serve its
parked spares (quota throttles creates, not starts); a Budget-dead provider
may not (waking a scale-to-zero spare resumes spend).

Sticky sessions: a spilled sandbox stays on its provider for its whole life;
fail-back affects new placements only. Checkpoint-based repatriation is
recorded future work.

## Two cost shapes, one ledger

`compute_meters` rows accrue by watermark (`rate × (now − last_accrued)`,
idempotent, catch-up-correct — tick frequency affects display freshness,
never totals):

- **fixed** — a managed host meters from create to destroy at
  `[placement.price]`'s hourly rate (unpriced ⇒ rate 0 + a one-time warn +
  "unpriced" in `placement list`). Attribution: dedicated host → worktree +
  zone + global; shared/packed host → `provider:` scope + global (fairly
  splitting a shared box's sunk cost across zones is explicitly v2).
- **metered** — spillover: `per_create_usd` one-shot at create;
  `active_hourly_usd` metered while the sandbox is awake (v1 approximation:
  while its worktree is open; scale-to-zero wake/suspend refinement later).

This split is what makes the pipeline's economics real: packed placement
projects ~0 marginal cost, autoscale projects a fixed-rate commitment,
spillover projects positive metered cost — and `check_compute_budget`
(scope → zone → global walk, kill-switch always refuses, mirror of the
proxy's `check_budget`) gates only the lanes that add spend. The Downgrade
analog is deferral: `Queue`/`Refuse` per `on_exhaustion`, never a silently
cheaper machine.

The ledger is SEPARATE from the proxy's by decision: two caps the user
manages independently, one shape so the mental model transfers
(`compute_budgets` is `proxy_budgets` minus token columns;
`sync_compute_budget_caps` is `zone::sync_budget_caps` pointed at the new
store).

## Presets: a code table expanding beneath explicit keys

`PlacementPreset::expand() -> PlacementOverlay` returns PREFERENCE keys only
(mode / pack_strategy / on_exhaustion / spillover order bias) — structurally
unable to widen a constraint. Expansion happens at the layer that set the
preset, beneath that same layer's explicit keys: explicit-beats-preset at one
layer, more-specific-beats-less across layers, and everything still flows
through the zone/repo clamps + the terminal mode floor. Bundle.extends was
rejected for this: it drags name resolution, cycle-breaking, and DB scope
pointers into what is a four-row table versioned with the binary.

## Zone + repo clamps (the hostile-repo gate)

`[zone.<name>.placement]`: `allowed_providers` (3-valued intersect),
`strictest_allowed_mode` (floor), `max_concurrent_sandboxes` (min),
`max_monthly_spend` (→ `zone:` compute scope). Repo `.superzej.*`
`[placement]`: preferences Allow; `spillover_provider_order` filtered to the
effective allow-list with ClampEvents; `allowed_providers` intersect-only;
every fleet-scoped key (overcommit, max_hosts, spend, autoscale, prices)
**Forbidden** with a loud denial — a repo narrowing fleet policy would change
packing for other repos' tenants on shared hosts, the same class of field as
`sandbox.backend`. The suite mirrors `classify_repo_overlay`'s hostile tests.

## Event loop / schema

No loop changes: accrual rides the existing self-throttled housekeeping
thread beside the reaper; verdicts evaluate inside the blocking placement
context. v36 is additive; `db.rs` stays at its cap via the established
comment-compression payment.
