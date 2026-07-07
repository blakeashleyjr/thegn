# Placement

## ADDED Requirements

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
