# Design

Load-bearing decisions; full type/flow detail lives with the implementation.

## Decision as a pure function

`thegn-core/src/scheduler.rs::decide_placement(request, inputs) ->
PlacementDecision` is total and deterministic over snapshot inputs (host
capacities + tenancies + policy + `now`), mirroring `render_plan::plan` /
`host_machine::step`. Lane order is fixed and trust runs **before** cost:

```
trust gate → pin bypass → dedicated (if required) → packed (ranked)
  → autoscale managed (ordered template lanes, cooldown-skipped)
  → spillover (typed now, executed in a later change) → queue/reject
```

Ineligibility is a typed reason enum (`NotReady`, `Cooldown`, `TrustClass`,
`ZoneCoTenancy`, `DedicatedOccupied`, `NoCapacity`, `WrongArch`,
`UnknownSpec`) so `placement explain` renders _why_, never a boolean.

## One resource view for every host — declared / reserved / measured

`host_capacity` carries three layers for ALL hosts, managed and independent:
the **declared** spec (authoritative for Managed — from the create template or
provider plan), the **reserved** totals (Σ live tenants' floors, derived from
`host_tenancy`), and the latest **measured** sample (a lightweight probe over
the host's existing control channel, TTL'd, refreshed lazily at decision/render
time — never idle polling). Display (`placement list`, the Hosts panel) and
assignment read the same rows, so what the user sees is exactly what the
broker decided on. Measured is a capacity _source_ only where no declared spec
exists (independent hosts, a later change); everywhere else it is display +
ranking hints.

## Capacity index

Integer math only (milli-cores / MiB — no floats in decisions).
`fits(cap, req)` = `reserved_floor + req.floor ≤ spec × overcommit_pct/100`
per axis; a host with an unknown spec **never packs** (it may still serve
Dedicated). `measured` load is observational (UI ranking hints), never a
capacity source, for Managed hosts — their spec is authoritative from create
time. Resource floors are **declared** per env (`[env.<n>.resources]`, default
`[placement.default_resources]`), not inferred: inference would make the
decision nondeterministic and untestable at the 95% core gate. Declared
ceilings feed the existing `SandboxLimits` only when the resolved spec has
none of its own.

## Reservation = the linearization point

`PlacementStore::tenancy_reserve` is a single guarded
`INSERT … SELECT … WHERE` over `host_tenancy` enforcing Σfloors ≤ ceiling,
zone co-tenancy (all tenants of a packed host share the request's zone), and
dedicated exclusivity — one statement, single-writer SQLite, therefore atomic
across processes (the same DB-as-arbiter pattern as `hosts.heartbeat`). Two
spawns racing for the last slot: one insert wins; the loser walks the
decision's ranked `alternates`, then re-snapshots (bounded, 3 rounds) before
falling to the exhaustion arm. A `ReservationGuard` (RAII) releases on any
failure path in-process; `tenancy_sweep_stale` (30 min TTL, maintainer tick)
catches crashed drivers. Lock ordering is unchanged: reservation is DB-level
and precedes `host_lock` → `sandbox_lock` (coarser-first, never nested).

Warm-pool spares hold tenancy for life: reserved at mint (zone taken from the
repo's workspace at mint time), `tenancy_rebind` at claim (same sandbox, same
host — amounts unchanged), released at destroy. No double-count, no window
where a spare is invisible to the index.

## Ownership axis

`HostOwnership { Managed, Independent }` lands as a type + `host_capacity`
column now. Only Managed hosts are ever created or destroyed by the engine;
`decide_scaledown` takes ownership as input and structurally never returns an
Independent host. (Full independent-host registration is the next change.)

## Autoscale

Trigger is **capacity-threshold at decision time**, inside the pure function —
zero queue-buildup latency and exhaustively testable — not a stateful
queue-depth watcher. Templates are ordered failover lanes
(`[[placement.autoscale.managed]]`, Hetzner `server_type` sizing): first
viable lane wins; a create failure stamps a `tpl:<provider>/<template>`
cooldown row in `placement_health` and the next spawn tries the next lane;
fail-back is automatic on cooldown expiry (the proxy router's
`is_exhausted`/`mark_exhausted` shape, mirrored not shared). Execution:
Hetzner create (labels `sz-placement=managed`) → `put_host_def` +
`capacity_put` (spec authoritative from the template) → the **unchanged**
`ensure_host_ready` drives Probing→Installing→Ready (`install_runtime =
"auto"` is legitimate: `autoscale.enabled` _is_ the consent — thegn created
the box). The VPS reaper gains one rule: `sz-placement=managed` instances with
no `host_capacity` row and age > orphan threshold are destroyed (crash between
POST and register). Scale-down runs from the existing lifecycle maintainer
tick via pure `decide_scaledown` (zero tenants + idle > threshold + count >
min_hosts); a ParkIdle pool spare holds a tenancy row, so its host is never
"idle" — deliberate warmth.

## Event loop / render invariants

Nothing here touches the event loop or the damage channels. The decision is a
pure in-memory function; every I/O step (snapshot reads, reservation, VPS
create, `ensure_host_ready`) runs in the existing spawn_blocking provisioning
contexts. UI updates ride the existing `HostUiTx` progress channel + waker
(chrome damage), exactly like host provisioning today. No new tick, no
idle-path polling.

## Schema

v34, additive, DDL in the new `db_placement.rs` (the `host_db::migrate_v30`
sibling pattern; `db.rs` carries only the version bump + one call —
net-zero lines, the file sits at the 3000 hard cap). `config.rs` (pinned)
gains one `placement` field + re-exports, paid for by extracting
`LifecycleConfig`/`EagerScope` to `config_env_tables.rs`.

## Engine-off invariant

`[placement] enabled = false` (default) short-circuits before any new code
path: `resolve_binding` behaves byte-identically, no tenancy rows are written,
smoke asserts the passthrough. Explicit `[env.<n>] host = "..."` pins bypass
the broker even when the engine is on (recorded as `mode='pinned'` tenancy so
accounting stays truthful).
