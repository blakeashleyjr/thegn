# Tasks

## 1. Spine (types, config, schema — no behavior change)

- [x] 1.1 Ratchet payment: extract `LifecycleConfig` + `EagerScope` from
      `config.rs` → `config_env_tables.rs` (pub use re-exports);
      `test/file-size-ratchet.sh --update` after the net shrink.
- [x] 1.2 Core `capacity.rs`: `HostOwnership`, `ResourceReq` (milli-cores/MiB
      integer math), cpu/mem grammar parsers (shared with `SandboxLimits`
      strings), `HostSpec`, `HostCapacity`, `fits()`, `utilization_permille()`
      — **unit tests**: grammar table incl. junk, fits boundary matrix (exact
      fit, +1 over, overcommit 100/150/200, per-axis independence,
      unknown-spec-never-fits).
- [x] 1.3 Core `config_placement.rs`: `[placement]` (enabled / mode /
      strictest_allowed_mode / pack_strategy / on_exhaustion / overcommit /
      default_resources / autoscale templates), `[env.<n>.resources]` +
      `placement_mode`, zone `placement_floor`; `Config.placement` field;
      `resolve_placement()` with the terminal mode-floor clamp —
      **unit tests**: TOML fixtures, defaults, floor-clamp lattice
      (auto+dedicated-floor ⇒ dedicated), unknown-value warns;
      `config/config.toml.example` section (drift test forces it).
- [x] 1.4 DB v34 `db_placement.rs` + `store/placement.rs` `PlacementStore`
      (capacity CRUD, guarded `tenancy_reserve`, activate/release/rebind,
      sweep, `placement_health` cooldowns, `placement_events`) — **unit
      tests**: round-trips, racing sequential reserves (second refused at the
      exact ceiling), zone/dedicated conflicts refused in-statement,
      migration idempotence.
- [x] 1.5 Core `scheduler.rs`: request/snapshot/decision types,
      `pack_eligible` (typed `Ineligible` reasons), `rank_hosts`
      (bin-pack/spread, deterministic ties), `clamp_mode` — **unit tests**:
      one-field-flipped reason matrix, rank orderings + tie-breaks.

## 2. Single-host packed placement

- [x] 2.1 `decide_placement` complete minus the `Provision` arm — **unit
      tests**: decision matrix over mode × strategy × host-state set × zone
      co-tenancy × arch × exhaustion action; determinism (same input twice ⇒
      identical output).
- [x] 2.2 Host `placement_flow.rs`: `place()` (pin bypass, snapshot,
      decide, reserve + ranked-alternates walk, `ReservationGuard`), hook at
      `host_flow::resolve_binding`, `tenancy_activate` at provision-marker
      write, release hooks in worktree-delete/sandbox-destroy —
      **mock tests**: pin records `pinned` tenancy; reservation released on
      `ensure_ready` failure; alternates walk after injected `NoCapacity`;
      engine-off passthrough.
- [x] 2.3 Pool tenancy seams: `tenancy_rebind` at claim + release at
      destroy are wired (no-ops until spares land on engine hosts — today's
      pool mints onto cloud providers, which are not engine machines; the
      mint-time reserve lands when the pool grows an engine-host lane).
- [x] 2.4 Sweep wiring (maintainer tick) + `thegn placement plan --json`
      (pure dry-run) + `placement list`; smoke: disabled engine ⇒
      passthrough asserted; enabled fixture ⇒ deterministic decision JSON.

## 3. Multi-host + autoscale

- [x] 3.1 `Provision` arm + ordered template lanes + cooldown skip/fail-back
      (`placement_health`) — **unit tests**: lane order, at-cap stop,
      cooldown skip, fail-back at expiry, spec-must-fit-request.
- [x] 3.2 Host `autoscale.rs`: Hetzner create (labels `sz-placement=managed`)
      → `put_host_def` + `capacity_put` → `ensure_host_ready`; reaper rule
      for unregistered managed orphans — **mock tests** (vps_mock pattern):
      create/register/ready path, create-failure stamps lane cooldown,
      orphan reaped.
- [x] 3.3 Scale-down: `decide_scaledown` (pure; never Independent) wired into
      the lifecycle maintainer tick; drain → provider destroy → `host_delete` + `capacity_delete` — **unit tests** incl. pool-spare-holds-host and
      min_hosts.
- [x] 3.4 Exhaustion surfacing: `queue` records the decision, warns, and
      the maintainer tick nudges when capacity may have freed (full
      auto-re-materialize deferred — re-open places the worktree); `reject`
      falls through recorded; `error` halts via SandboxHalt. Delta spec
      merged; `just ci`.
