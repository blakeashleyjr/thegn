# Placement

## ADDED Requirements

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
