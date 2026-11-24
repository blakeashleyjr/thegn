# Tasks

Each task is a work item of workstream WS01 (wave W1) unless noted. Tick a task when its item merges.

## 1. Capability and egress truth

- [x] 1.1 WS01-P1-01: per-kind provider facts in core (`egress_api`, `checkpoints`, `files`) and
      `ProviderCaps::for_kind`
- [x] 1.2 WS01-P1-02: `egress_for(&EgressInputs)` and honest capability fields (`Offline`, no false
      `Enforce`, VPN is `Unmanaged`)
- [x] 1.3 WS01-P1-03: matrix relabel (`NetIsolation::{OwnNetns, Unverified}`)
- [x] 1.4 WS01-P1-04: refuse Apple `network = "none"` with `network-unexpressible`
- [x] 1.5 WS01-P1-05: warn when provider placements ignore hostname rules
- [x] 1.6 WS01-P1-06: `network_audit` truth (and prior audit F14 closure)
- [x] 1.7 WS01-P1-07: VPN DNS mode truth (`filter-front` reserved)
- [x] 1.8 WS01-P1-08: stable clamp drops k8s envs

## 2. `dns_filter` fate

- [x] 2.1 WS01-P2-01: create `thegn_core::egress::pattern` with today's semantics
- [x] 2.2 WS01-P2-02: delete the DNS filter server, its tests, recipe and CI run line

## 3. Reserved kinds

- [x] 3.1 WS01-P3-01: reserve `smol` and `appcontainer`; reserved kinds are never runtime candidates
- [x] 3.2 WS01-P3-02: smol cleanup (labels, dead docker-clone arms, lists)

## 4. Remote truth

- [x] 4.1 WS01-P4-01: Daytona maintenance flag, doctor line and config example
- [x] 4.2 WS01-P4-02: Fly stale comments and config text
- [x] 4.3 WS01-P4-03: ssh `ControlMux` (Windows off, long paths off)

## 5. Docs, spec and posture

- [x] 5.1 WS01-P5-01: `docs/ARCHITECTURE.md` truth (seam deviations, gate line)
- [ ] 5.2 WS01-P5-02: help, README, config-example and wording sweep
- [x] 5.3 WS01-P5-04: fold and archive `mark-unverified-backends` and append its reconciling MODIFIED block
- [ ] 5.4 WS01-P5-05: security-model help page

## 6. Persistence channels

- [ ] 6.1 WS01-P6-01: sealed binds no shared read-write host cache
- [ ] 6.2 WS01-P6-02: standard drops read-write `.keychain` and `.claude-profiles`; agent executable
      config pinned read-only

## 7. Close out

- [ ] 7.1 WS01-P5-03: sync and archive `fix-truth-in-reporting` at the end of W1 (after 5.3), rebasing the
      MODIFIED blocks on the current specs
- [ ] 7.2 Run `just ci` (includes `openspec validate --all --strict`) once, when the wave is ready to land
