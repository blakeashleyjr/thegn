# Add independent hosts to the placement pool (probes, trust classes, drain)

## Summary

Fold **independent hosts** — machines the user owns and pointed thegn at
(`[host.<name>]` with ssh/iroh/local reach) — into the placement engine's pool
as first-class, resource-managed citizens, without ever granting the engine a
destroy handle over them. Three additions make that safe and useful: a
**headroom probe** (one cheap exec over the host's existing control channel)
that gives every host a measured resource sample — and gives spec-less
independent hosts their only capacity source, behind a compounding-uncertainty
haircut; a **trust-class ladder** (T0 host-shell < T1 container < T2 rootless
container < T3 guest kernel) projected from what the probe actually found,
with independent hosts defaulting **one notch below** an equivalently-equipped
managed host unless the user explicitly attests
(`trust_egress_enforced = true`); and a **drain** lifecycle
(`thegn host drain`) so de-registering a box with live sandboxes parks it
out of every candidate list and finalizes only when the last tenant leaves —
never a forced kill, never an implicit cleanup of someone else's machine.

## Impact

- **Config** — `[host.<name>]` gains `trust_egress_enforced` (attestation;
  profile-locked like every host key by the existing structural repo
  exclusion). `[placement]` gains `independent_safety_pct` (the haircut) and
  `headroom_ttl_secs`.
- **DB** — `user_version` bump to **35**: `hosts` gains `headroom_json` /
  `last_headroom` (additive ALTERs, the `config_json` precedent; the trust
  class is recomputed from caps + attestation, never persisted).
- **Core** — new `trust_class.rs` (the ladder + one-notch-down + attestation
  raise, projected over the probed `HostCaps`), new `host_probe.rs`
  (`Headroom` KEY=VALUE parser + `independent_effective_ceiling`); the
  scheduler's pack gate upgrades from the interim runtime/rootless booleans to
  `effective trust ≥ required trust`.
- **svc** — `PROBE_SCRIPT` extended (nproc / MemTotal / cgroup v2 / userns /
  bwrap); a new `HEADROOM_SCRIPT` + `probe_headroom` on the host runner, with
  the script↔parser contract test extended to cover it.
- **Flows** — placement refreshes a candidate host's headroom lazily (TTL'd,
  at decision time — never the idle ticker); `thegn host drain <name>` +
  `host rm` refusing/draining with live tenants; the Hosts/placement views
  show the measured layer for every host.
- **tasks.md**: group J (remote access), AE (container provisioning), 244
  (fleet view — the measured layer completes its per-host data source).

## Rationale

The engine (add-placement-engine) packs onto hosts with declared specs and
structurally never destroys anything it didn't create — but an independent
host today is a black box: no measured load, a config-declared size taken on
faith, and a pack gate that can't tell a hardened box from a bare one. Every
mechanism this change adds exists precisely because thegn does NOT control
these machines: the probe is the only capacity source available, the haircut
compounds two guesses (declared × overcommit) conservatively, the trust notch
encodes that a probe proves presence but never enforcement, and drain-not-kill
respects that the sandboxes on that box may be hours-deep agent sessions on
hardware thegn is a guest on.

## Non-goals

- **Autoscaling independent hosts** — they are static; a human adds and
  removes them.
- **Verifying the attestation** — `trust_egress_enforced` is taken on faith
  by design; collapsing the asymmetry would mean inferring a guarantee a
  probe cannot confirm.
- **Migration of running sandboxes off a draining host** — drain waits;
  `--force` stops thegn-labelled containers only, explicitly.
- **A degraded bare-SSH participation tier** — an independent host walks the
  same full state machine to `Ready`; inline `[env.*.ssh]` already covers
  "just exec over ssh".
