# Design

## One trust ladder, projected — never a second one

`capabilities.rs` already owns the honest isolation vocabulary
(`IsolationClass`, `EgressKind`). `trust_class.rs` PROJECTS it into a total
order for co-tenancy decisions:

```
T0 HostShell < T1 Container < T2 RootlessContainer < T3 GuestKernel
```

Derivation from a probed runtime: none ⇒ T0; docker / rootful podman ⇒ T1;
rootless podman ⇒ T2; Apple container / microVM runtimes ⇒ T3. A **Managed**
host's class stands as derived (superzej built the stack from a pinned image —
presence ≈ enforcement). An **Independent** host's _effective_ class is one
notch down unless `trust_egress_enforced = true`: the probe can prove podman
exists, it cannot prove the box enforces the egress/config posture a
superzej-built image guarantees — the attestation is the user accepting that
gap on their own machine, taken on faith by design. Required class per
request: sealed-profile sandboxes pack at ≥ T2, everything else ≥ T1;
dedicated placements have no class requirement (exclusivity is the boundary).

## Headroom: the probe that serves display everywhere and capacity only where nothing better exists

`HEADROOM_SCRIPT` (one exec over the existing runner channel): nproc,
MemTotal/MemAvailable, load1, disk free, running containers. Parsed by pure
`host_probe::parse_headroom` (KEY=VALUE, same discipline + contract test as
`HostCaps::parse_probe`). Freshness: refreshed lazily when a placement
decision (or an explicit CLI/panel view) finds the stored sample older than
`headroom_ttl_secs` (default 60s) — never on the 2s ticker (an idle superzej
must not exec into every registered box forever). Persisted on the `hosts` row
so a fresh process starts warm, and mirrored into `host_capacity.measured` so
every resource surface (placement list, panel, broker ranking hints) reads one
table.

For a MANAGED host the sample is observational only — spec is authoritative.
For an INDEPENDENT host the sample is the _only_ honest capacity input, so the
effective packing ceiling compounds the uncertainty conservatively:

```
ceiling = min(declared capacity, probed total) × overcommit × safety_pct
```

(`independent_safety_pct`, default 85 — an uncontrolled box carries invisible
co-workloads and superzej has no eviction lever there), plus a live gate:
pack only while `MemAvailable × safety > requested floor`. A probe failure
makes the host pack-ineligible this round (fail closed); Dedicated stays
allowed while the host is Ready.

## Drain, never kill

`HostState::Draining` is a new durable tag; `step()` stays total (a draining
host absorbs every event without moving — nothing re-provisions it). The
broker excludes draining hosts from every lane. `superzej host drain <name>`
flips the state; `host rm` on a host with live tenants drains instead of
deleting and reports what is still running; `--force` stops
superzej-labelled containers over the control channel first. Finalize (row +
inventory delete) happens only at zero live tenants, and on-host artifacts
are only ever _offered_ for cleanup — never silently removed from a machine
the user owns.

## Schema / invariants

v35: two additive `hosts` ALTERs (`headroom_json`, `last_headroom`), the
tolerated-ALTER precedent. The trust class is never persisted — it is a
cheap pure projection of probed caps + config attestation, recomputed at
snapshot time so it can't go stale. No event-loop change: probes run
in the same blocking placement/CLI contexts as everything else; the idle loop
never polls. `db.rs` stays at its cap via comment compression (the v30
pattern).
