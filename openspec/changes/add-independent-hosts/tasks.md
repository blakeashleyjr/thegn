# Tasks

## 1. Pure core

- [ ] 1.1 `trust_class.rs`: the T0–T3 ladder, derivation from probed
      `HostCaps`, one-notch-down for independent hosts, attestation raise,
      `required_trust(profile)` — **unit tests**: full derivation table,
      notch/attestation matrix incl. T0 fixpoint, ordering totality.
- [ ] 1.2 `host_probe.rs`: `Headroom` + `parse_headroom` (KEY=VALUE) +
      `independent_effective_ceiling` (min-of-claims × overcommit × safety) +
      the live MemAvailable gate — **unit tests**: full/partial/junk parses,
      min-of-claims, safety clamp, neither-source ⇒ None, live-gate edges.
- [ ] 1.3 `host_machine.rs`: `Draining` durable state (tag round-trip,
      absorb-everything totality, resume no-op) — **unit tests**.
- [ ] 1.4 `host.rs`: `HostCaps` gains cgroup_v2 / userns / nproc /
      mem_total_kb (serde defaults keep pre-v35 caps_json readable) —
      **unit tests**: probe parse + back-compat round-trip.
- [ ] 1.5 Scheduler trust upgrade: `HostSnapshot.trust` +
      `PlacementRequest.required_trust` replace the interim
      runtime/rootless booleans — **unit tests** updated (reason matrix
      keeps `TrustClass` semantics).
- [ ] 1.6 Config: `[host.<n>] trust_egress_enforced`, `[placement]
independent_safety_pct` + `headroom_ttl_secs`;
      `config.toml.example` docs (drift test) — **unit tests**.
- [ ] 1.7 DB v35: `hosts` ALTERs + `HostStore` setters/getters for
      headroom + trust — **unit tests**: round-trips, pre-v35 rows read
      clean, migration idempotence.

## 2. svc + flows

- [ ] 2.1 `PROBE_SCRIPT` extension + `HEADROOM_SCRIPT` +
      `HostRunner::probe_headroom` — extend the script↔parser contract
      test to the headroom pair.
- [ ] 2.2 Placement-time lazy headroom refresh (TTL'd) feeding
      `host_capacity.measured` for EVERY host + the independent ceiling for
      spec-less ones; probe failure ⇒ pack-ineligible this round.
- [ ] 2.3 Drain flow: `superzej host drain <name>` /
      `host rm` drains-with-live-tenants (+ `--force` stops
      superzej-labelled containers); broker + snapshot honor `Draining`;
      finalize at zero tenants — **mock tests**.
- [ ] 2.4 Surfaces: placement list / Hosts panel show trust class +
      measured age for every host; smoke: drain excludes a host from
      `placement plan`; `just ci`.
