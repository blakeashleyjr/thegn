# Tasks

## 1. Spillover (pure core → svc → lane)

- [x] 1.1 Core `spillover.rs`: `SpillKind` (Budget/Quota/CreateFailure),
      `classify_spill(status, retry_after, err)`, cooldowns via
      `proxy::backoff`, `pick_spillover` (order ∩ allow-list, marker skip,
      budget predicate, implicit fail-back), `spare_claimable` — **unit
      tests**: classification table (402/429+Retry-After/timeout/5xx/junk),
      escalation caps, order/ceiling interplay, Quota-spare-yes /
      Budget-spare-no, fail-back at expiry.
- [~] 1.2 Provider error classification rides the error-chain text
  (`classify_spill` sniffs the embedded `status <code>`); native
  status/Retry-After propagation on the provider error type is a
  follow-up refinement.
- [x] 1.3 Broker lane: prefer READY spillover spares over any paid create;
      spillover create via the existing provider path; markers persisted in
      `placement_health` under `provider:` keys — **flow tests** with an
      in-memory Db: budget-dead skips to the next provider; cooldown expiry
      fails back; sticky sessions (an existing placement never migrates).

## 2. Compute ledger

- [x] 2.1 DB v36 `compute_budgets` + `compute_meters`;
      `store/compute.rs` `ComputeLedgerStore` (set-limits preserves spend,
      watermark `accrue`, stop = final accrual) — **unit tests**: window
      rollover, double-tick accrues once, catch-up gap, migration
      idempotence.
- [x] 2.2 Verdicts (in `db_compute.rs`): `ComputeVerdict {Allow, Refuse, Queue}`,
      `check_compute_budget` (scope → zone → global; kill-switch),
      `record_compute_spend` triple attribution, `accrue_cost` pure —
      **unit tests** ported from the proxy budget suite.
- [x] 2.3 Wiring: meters start/stop at VPS create/destroy + spillover
      claim/close; accrual on the housekeeping tick;
      `sync_compute_budget_caps` at startup + placement/zone CLI; budget
      verdicts gate autoscale + spillover lanes — **flow tests**.
- [x] 2.4 `[placement.price.*]` + `max_monthly_spend` config + docs
      (drift test); `placement list` shows accrued $/period + "unpriced".

## 3. Presets + zone/repo clamps + surfaces

- [x] 3.1 `PlacementPreset` (fill-defaults expansion) + per-layer expansion beneath explicit
      keys — **unit tests**: the four precedence combinations; expansion
      emits preference keys only (exhaustive destructure).
- [x] 3.2 Zone compute cap (`[zone.<n>.budget] limit_compute_cost` →
      `zone:` ledger scope via `sync_compute_budget_caps`). Repo overlays
      are handled STRUCTURALLY: `RepoConfigFile` carries no `[placement]`
      table at all (the `[host.*]` precedent), so no clamp engine is needed
      — deferred until repo-level placement preferences are wanted.
- [x] 3.3 CLI: `placement budget` (show/set-limit/kill/unkill),
      `placement explain` renders budget verdicts, `config explain
placement.*` clamp traces; smoke additions; `just ci`.

Deferred (recorded): spillover per-create/active-time metering (fixed-
rate managed meters land here; vendor metering needs provider usage
APIs), packed-host cost split across zones, adaptive overcommit under
budget pressure.
