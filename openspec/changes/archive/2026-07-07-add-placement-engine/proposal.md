# Add the placement engine (dedicated / packed / autoscaled hosts)

## Summary

Give every sandbox spawn a **placement decision**: run **Dedicated** (one
sandbox, exclusive host), **Packed** (bin-packed with other sandboxes onto a
shared Ready host via thegn's own container engine), or — in later changes —
spill to an external sandbox vendor. A pure, exhaustively-tested broker
(`thegn-core::scheduler::decide_placement`) ranks the host pool through a
capacity index (declared floors vs. spec × overcommit ceilings) behind a trust
gate (isolation class + zone co-tenancy checked **before** cost), reserves the
chosen slot atomically in SQLite, and hands the existing `ensure_ready` the
winning `HostBinding`. On pool exhaustion the engine can **autoscale**: create
a new Managed VPS from an ordered template list (Hetzner first), drive it to
`Ready` through the existing host state machine, and scale it back down when it
drains.

The economics this encodes: commodity VMs bill for existence at a fraction of
sandbox-vendor rates, and thegn already owns the isolation layer vendors
charge for — so the default is _pack onto hosts you already pay for first_,
create new capacity second, and (future change) pay vendor markup only as the
spillover of last resort.

## Impact

- **Config** — new global/profile-only `[placement]` table (mode /
  pack_strategy / overcommit / on_exhaustion / autoscale templates), a
  per-env `[env.<name>.resources]` declaration (cpu/memory floors + optional
  ceilings), `[env.<name>] placement_mode`, and a zone-level
  `placement_floor`. Documented in `config/config.toml.example` (drift-tested).
- **DB** — `user_version` bump to **34**: `host_capacity` (ownership, spec,
  overcommit), `host_tenancy` (the atomic reservation ledger: sandbox → host
  with reserved floors), `placement_health` (cooldown markers for autoscale
  template lanes, later spillover providers), `placement_events` (decision
  traces for `placement explain`). New `store/placement.rs` `PlacementStore`.
- **Spawn path** — one hook where `host_flow::resolve_binding` resolves an
  env's binding: engine off (default) or pinned env ⇒ byte-identical old path;
  engine on ⇒ `placement_flow::place()` chooses the host, reserves tenancy,
  and returns a `HostBinding` the unchanged `ensure_host_ready` drives.
- **CLI** — `thegn placement plan --json` (pure dry-run of the decision, the
  smoke/debug surface), `thegn placement list` (hosts, capacity, tenants).
- **tasks.md**: group AE (container provisioning: 385 CoW/base image, 386
  prewarmed pool, 392 image build cache), group J (remote access), 749
  (commodity-VPS backend), 244 (fleet view groundwork — capacity/tenancy rows
  are the data source it will render).

## Rationale

thegn already has every layer this composes — `[host.*]` bindings, the pure
host state machine + single-flight driver, the warm-spare pool, the VPS
(Hetzner) provider with intent-ledger + reaper, and zone trust ceilings. What
is missing is the _decision_: nothing today maps sandbox → host with resource
accounting, so a second worktree either re-derives its own env or lands
wherever its env statically points. The broker fills exactly that gap, in the
house style (decision-as-pure-function, like `render_plan::plan` and
`host_machine::step`), leaving all I/O in the existing impure drivers.

## Non-goals

- **Independent (user-owned SSH) hosts in the scheduler pool** — the ownership
  axis lands here as a type (`Managed`/`Independent`), but registration,
  probing, and trust asymmetry are the `add-independent-hosts` change.
- **Spillover to external vendors and the compute spend ledger** — the
  decision enum carries the arms; execution is the
  `add-spillover-and-compute-ledger` change.
- **Migration/rebalancing of running sandboxes** — sessions stay where they
  were placed.
- **Changing any spawn behavior while `[placement] enabled = false`** (the
  default) — the engine is strictly additive.
