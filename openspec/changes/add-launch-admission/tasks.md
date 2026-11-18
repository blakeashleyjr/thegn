# Tasks

Implemented in waves W3 and W4 (handle in W3, refusal table in W4); nothing below is started. Tick a task when
its item merges. The owning programme item is named on each line.

## 1. Types and handle

- [x] 1.1 `LaunchClass`, `UntrustedKind`, `HostTrustKind`, admission vocabulary stubs (WS00-P1-13)
- [ ] 1.2 `admission::evaluate` and `LaunchAdmission` with private fields; re-key `LAUNCH_ROUTES` by class (WS02-P5-08)
- [ ] 1.3 Storage-support manifest column and ephemeral refusal (WS02-P5-07)
- [ ] 1.4 `EffectivePolicyId`, the `thegn.policy` label and reuse by equality (WS02-P5-07)

## 2. Routes through the handle

- [ ] 2.1 Interactive panes, terminals and devcontainer migrate to `LaunchAdmission` (WS02-P5-08)
- [ ] 2.2 Queue, PR, CI and review agents, `AgentRun` (WS11-P3-11)
- [ ] 2.3 Merge gate, regeneration, bisect, test discovery (WS11-P3-12)
- [ ] 2.4 Autopilot (WS11-P3-14); Tests panel runs (WS11-P3-15)
- [ ] 2.5 Repo lifecycle scripts and headless `tools.run` (WS11-P3-16); language services (WS11-P3-13)
- [ ] 2.6 Untrusted plugins (WS10-P3-02); provider exec (WS08-P7-05)
- [ ] 2.7 One `no_host_spawn_after_refusal_<route>` test per route; its `LAUNCH_ROUTES` row reads `handle`

## 3. Refusal table and corpus

- [ ] 3.1 Refusal table with a no-host-spawn test per row (WS02-P5-09)
- [ ] 3.2 Canary corpus by class and backend (WS02-P5-10)

## 4. Docs and validation

- [ ] 4.1 Help pages and `thegn doctor` rows for classes, routes and refusals
- [ ] 4.2 Run `just ci` (includes `openspec validate --all --strict`) once, when the implementation is complete
