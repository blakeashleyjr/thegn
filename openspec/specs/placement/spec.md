# placement Specification

## Purpose

TBD - created by archiving change add-placement-engine. Update Purpose after archive.

## Requirements

### Requirement: Placement decisions are pure, deterministic, and explainable

superzej SHALL compute every placement decision (dedicated / packed /
provision / queue / reject) as a pure function of a snapshot of host
capacities, tenancies, policy, and the clock — the same inputs MUST always
produce the same decision — and SHALL record each decision with per-candidate
ineligibility reasons so a user can ask why a sandbox landed where it did.

#### Scenario: Same snapshot, same decision

- **WHEN** `decide_placement` is evaluated twice over an identical snapshot
- **THEN** it returns an identical decision, including the ranked alternates

#### Scenario: A decision trace is recorded

- **WHEN** the engine places (or refuses to place) a sandbox
- **THEN** a placement event is persisted carrying the decision, the chosen
  target, and each candidate host's fits/trust-gate outcome

### Requirement: Trust gates packing before cost

superzej SHALL evaluate isolation and zone constraints before any
capacity or cost ranking: a sandbox MUST NOT be packed onto a host whose
isolation class does not satisfy the request's requirements, and MUST NOT be
packed onto a host whose current tenants belong to a different zone,
regardless of free capacity.

#### Scenario: Zone co-tenancy refused despite free capacity

- **WHEN** a request in zone `clientA` targets a host with ample free
  capacity whose only tenant belongs to zone `clientB`
- **THEN** the host is ineligible with reason `ZoneCoTenancy` and the engine
  moves to the next lane

### Requirement: Capacity reservations are atomic across processes

superzej SHALL reserve a sandbox's declared resource floor on its chosen host
via a single guarded SQL statement enforcing the capacity ceiling, zone
co-tenancy, and dedicated exclusivity, so that two concurrent spawns racing
for the last slot resolve to exactly one winner; the loser MUST re-rank and
retry bounded times before falling to the exhaustion policy. A reservation
whose driver dies MUST be swept after a TTL.

#### Scenario: Two spawns race for the last slot

- **WHEN** two processes decide `Packed` onto the same nearly-full host
  concurrently
- **THEN** exactly one reservation succeeds and the other walks its ranked
  alternates (or provisions/queues per policy)

### Requirement: Hosts with unknown specs never pack

superzej SHALL treat a host without an authoritative resource spec as
ineligible for packing (it MAY still serve a dedicated placement), because an
overcommit ceiling computed from an unknown base is meaningless.

#### Scenario: Spec-less host skipped for packing

- **WHEN** the pool contains a Ready host whose capacity row has no cpu/mem
  spec
- **THEN** packing skips it with reason `UnknownSpec` while a dedicated
  request may still choose it

### Requirement: Autoscale creates Managed hosts through ordered template lanes with fail-back

superzej SHALL, when no eligible host exists and autoscale is enabled and
under its ceilings, provision a new Managed host from the first viable entry
of the ordered template list; a create failure SHALL cool that lane down and
subsequent requests SHALL try the next lane, with the cooled lane
automatically eligible again when its cooldown expires. Budget/count ceilings
(`max_hosts`, per-template `max`) MUST be enforced at decision time.

#### Scenario: Lane failure fails over and fails back

- **WHEN** the first template lane's create call fails
- **THEN** the lane is cooled down, the next request provisions from the
  second lane, and after the cooldown expires the first lane is tried again

### Requirement: Only Managed hosts are ever destroyed

superzej MUST NOT destroy or scale down a host it did not create: scale-down
decisions SHALL be structurally restricted to Managed hosts, and an
Independent host has no destroy capability by construction.

#### Scenario: Idle independent host survives scale-down

- **WHEN** scale-down evaluates a pool containing an idle Independent host
  and an idle Managed host past the idle threshold
- **THEN** only the Managed host is destroyed

### Requirement: The engine is inert when disabled and bypassed by pins

superzej SHALL keep every existing spawn path byte-identical while
`[placement] enabled = false` (the default), and SHALL bypass the broker for
an env explicitly pinned to a host (`[env.<n>] host = "..."`) while still
recording the pinned tenancy so capacity accounting stays truthful.

#### Scenario: Disabled engine changes nothing

- **WHEN** a worktree materializes with placement disabled
- **THEN** binding resolution, provisioning, and spawn behave exactly as
  before the engine existed and no tenancy rows are written

#### Scenario: Pinned env bypasses scheduling but is accounted

- **WHEN** the engine is enabled and an env pins `host = "gpu-box"`
- **THEN** the sandbox lands on `gpu-box` without consulting the broker and a
  `pinned` tenancy row records its resource floor there

### Requirement: Every host's resources are visible in one place — declared, reserved, and measured

superzej SHALL maintain a per-host resource view covering ALL hosts — managed
and independent alike — with three layers: the declared spec (authoritative
for managed hosts), the reserved totals (Σ of live tenants' floors from the
tenancy ledger), and the latest measured sample (observational load from a
lightweight probe over the host's existing control channel). The view MUST be
renderable (`szhost placement list`, the Hosts panel) and MUST be the same
data the broker snapshots for assignment — display and scheduling never
diverge. Measured samples SHALL refresh lazily with a TTL at decision/render
time, never via idle polling (the 0%-idle invariant).

#### Scenario: placement list shows all three layers for every host

- **WHEN** the user runs `szhost placement list` with a managed VPS and a
  pinned ssh host registered
- **THEN** each row shows the declared spec (or "unknown"), reserved
  cpu/mem + tenant count, and the last measured load with its age

#### Scenario: Measured load never overrides a managed spec for assignment

- **WHEN** a managed host's measured sample momentarily exceeds its reserved
  totals
- **THEN** the fits decision still evaluates against declared floors and the
  spec ceiling (measured is display + ranking hints only)

### Requirement: Warm-pool spares occupy capacity for their whole life

superzej SHALL reserve tenancy for a pool spare when it is minted, rebind the
same reservation to the claiming worktree at claim time, and release it only
at destroy — a spare MUST never be invisible to the capacity index and MUST
never be double-counted across the claim transition.

#### Scenario: Claim transition keeps one reservation

- **WHEN** a ready spare is claimed by a new worktree
- **THEN** the host's reserved totals are unchanged by the claim (the row is
  rebound, not re-added)

### Requirement: Independent hosts join the pool with probed capacity behind a conservative haircut

superzej SHALL admit user-owned (`[host.*]` ssh/iroh/local) machines to the
packing pool only with a capacity source: the declared `capacity` and/or a
headroom probe over the host's existing control channel. For these hosts the
effective packing ceiling MUST compound the uncertainty conservatively —
`min(declared, probed) × overcommit × safety` — and packing MUST additionally
require live available memory above the requested floor. A failed probe makes
the host pack-ineligible for that decision (dedicated placement MAY still
proceed on a Ready host).

#### Scenario: Declared size is capped by the probed reality

- **WHEN** an independent host declares `capacity = { cpu = "16", memory = "64g" }`
  but the probe reports 8 cores / 32 GiB total
- **THEN** the packing ceiling derives from the probed 8 / 32 (times
  overcommit and the safety factor), not the declaration

#### Scenario: Probe failure fails closed for packing only

- **WHEN** a candidate independent host's headroom probe errors during a
  placement decision
- **THEN** the host is ineligible for packing that round while remaining
  eligible for a dedicated placement if Ready

### Requirement: Trust classes gate co-tenancy, with independent hosts one notch down unless attested

superzej SHALL rank co-tenancy boundaries on one ladder (host shell <
container < rootless container < guest kernel), derive a host's class from
what the probe actually found, and require sealed-profile sandboxes to pack
only at rootless-container strength or above. An independent host's effective
class MUST default one notch below the derived class — a probe proves
presence, never enforcement — and MUST be restorable only by the explicit
per-host attestation `trust_egress_enforced = true`, which is taken on faith
and never verified.

#### Scenario: Same probe result, different effective trust

- **WHEN** a managed host and an unattested independent host both probe
  rootless podman
- **THEN** the managed host packs sealed-class sandboxes and the independent
  host does not, until its owner attests

#### Scenario: Attestation restores parity

- **WHEN** the owner sets `trust_egress_enforced = true` on that independent
  host
- **THEN** its effective class equals the derived class and sealed-class
  packing becomes eligible

### Requirement: De-registration drains — it never kills or cleans another machine silently

superzej SHALL move a de-registered host to a durable `draining` state that
excludes it from every placement lane while existing sandboxes run to
completion, finalizing (forgetting state + inventory) only at zero live
tenants. Forced removal SHALL stop only superzej-labelled containers, only on
explicit request, and on-host artifacts MUST only ever be offered for cleanup,
never removed implicitly.

#### Scenario: Drain with live tenants parks the host

- **WHEN** `superzej host drain gpu-box` runs while two sandboxes live there
- **THEN** the host disappears from placement candidates immediately, both
  sandboxes keep running, and the host finalizes only after both release

#### Scenario: Draining survives restarts

- **WHEN** superzej restarts while a host is draining
- **THEN** the host resumes in `draining` (never re-provisions) and stays out
  of the pool

### Requirement: The measured resource layer covers every host without idle polling

superzej SHALL refresh a host's measured sample lazily — at placement decision
or explicit view time, guarded by a TTL — over the host's existing control
channel, persist it, and render it in the per-host resource view alongside the
declared and reserved layers. The idle event loop MUST NOT probe hosts.

#### Scenario: Stale sample refreshes at decision time

- **WHEN** a placement decision considers a host whose sample is older than
  the TTL
- **THEN** one headroom exec refreshes it before eligibility is evaluated,
  and an idle superzej issues no probes at all

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
