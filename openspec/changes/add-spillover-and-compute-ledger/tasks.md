# Tasks

## 1. Spillover (pure core → svc → lane)

- [ ] 1.1 Core `spillover.rs`: `SpillKind` (Budget/Quota/CreateFailure),
      `classify_spill(status, retry_after, err)`, cooldowns via
      `proxy::backoff`, `pick_spillover` (order ∩ allow-list, marker skip,
      budget predicate, implicit fail-back), `spare_claimable` — **unit
      tests**: classification table (402/429+Retry-After/timeout/5xx/junk),
      escalation caps, order/ceiling interplay, Quota-spare-yes /
      Budget-spare-no, fail-back at expiry.
- [ ] 1.2 svc: provider create errors carry HTTP status + Retry-After
      (Sprites/Daytona) — **mock fixtures**: 402 and 429 responses surface
      classifiable errors.
- [ ] 1.3 Broker lane: prefer READY spillover spares over any paid create;
      spillover create via the existing provider path; markers persisted in
      `placement_health` under `provider:` keys — **flow tests** with an
      in-memory Db: budget-dead skips to the next provider; cooldown expiry
      fails back; sticky sessions (an existing placement never migrates).

## 2. Compute ledger

- [ ] 2.1 DB v36 `compute_budgets` + `compute_meters`;
      `store/compute.rs` `ComputeLedgerStore` (set-limits preserves spend,
      watermark `accrue`, stop = final accrual) — **unit tests**: window
      rollover, double-tick accrues once, catch-up gap, migration
      idempotence.
- [ ] 2.2 Core `compute_spend.rs`: `ComputeVerdict {Allow, Refuse, Queue}`,
      `check_compute_budget` (scope → zone → global; kill-switch),
      `record_compute_spend` triple attribution, `accrue_cost` pure —
      **unit tests** ported from the proxy budget suite.
- [ ] 2.3 Wiring: meters start/stop at VPS create/destroy + spillover
      claim/close; accrual on the housekeeping tick;
      `sync_compute_budget_caps` at startup + placement/zone CLI; budget
      verdicts gate autoscale + spillover lanes — **flow tests**.
- [ ] 2.4 `[placement.price.*]` + `max_monthly_spend` config + docs
      (drift test); `placement list` shows accrued $/period + "unpriced".

## 3. Presets + zone/repo clamps + surfaces

- [ ] 3.1 `PlacementPreset::expand()` + per-layer expansion beneath explicit
      keys — **unit tests**: the four precedence combinations; expansion
      emits preference keys only (exhaustive destructure).
- [ ] 3.2 `ZonePlacement` (`[zone.<n>.placement]`) + `apply_zone_placement`;
      repo `[placement]` overlay + `classify_repo_placement` — **the
      hostile-repo suite**: cannot widen providers, cannot set
      overcommit/spend/hosts/autoscale/prices (loud Forbidden denials),
      spillover order filtered with events, 3-valued list semantics.
- [ ] 3.3 CLI: `placement budget` (show/set-limit/kill/unkill),
      `placement explain` renders budget verdicts, `config explain
placement.*` clamp traces; smoke additions; `just ci`.
