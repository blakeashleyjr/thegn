# One launch class and one admission handle for every repo-influenced spawn

## Summary

THE-791 (owner decision 2026-10-13, programme rulings 2026-11-17) requires that every spawn of repo-influenced
code has a launch class, that headless and untrusted launchers are admitted through one handle, and that an
unsupported or unverified control refuses admission instead of degrading to a host spawn. This change writes
that as requirements. It is the contract THE-791 gates 3 (one admission handle) and 4 (adapter contract and
refusal table) are implemented against. It changes no code itself. Sources: `docs/audits/sandbox-security-2026-10-13/design.md`
("Runtime/launcher contract and audit"), the acceptance text of THE-794 and THE-795, and the THE-791
alignment decisions T1 to T3, T7, T8 and T11.

Today `sandbox_admission::Admission` (THE-1202) is an opaque marker carried by agent outcomes, and
`LAUNCH_ROUTES` (THE-1225) lists the routes; it does not yet validate mounts, home view, environment channels,
network hash, capabilities, limits or process scope. This change extends it rather than adding a parallel type.

## What changes

- Every spawn of repo-influenced code carries one `LaunchClass`: `InteractiveOwner`, `Untrusted(kind)` or
  `HostTrust(kind)`, orthogonal to `LaunchKind` (PTY versus command).
- Untrusted launches are admitted only through one handle, `LaunchAdmission`, whose private fields make a
  refused request unable to reach a spawn.
- Adapters (built-in, shipped, config-defined, plugin, provider) report enforced controls, computed from the
  manifest, the host report and, for non-built-ins, a live attestation, plus an immutable effective-policy
  identity; reuse requires equality.
- Backends that cannot qualify refuse secure policies (a refusal table with a test per row).
- A secure refusal never becomes a host spawn.

## Non-goals

- Private-copy workspaces, capture and sync (change `add-private-workspaces`).
- Network presets, the egress proxy and the resolution order of presets (workstream WS04).
- The operation policy engine in front of `tools.run` (change `add-sandbox-policy-engine`; it adds its rules
  before admission and never widens a ceiling).
- Changing any default of the user's own interactive panes. They keep the `standard` profile, an accident
  guard and not a boundary (owner decision 2026-11-05). The untrusted defaults per class are set by the
  gate-5 flip (WS11-P6-05), not by this change.
- The reporting corrections of `fix-truth-in-reporting`.

## Impact

- **Roadmap:** `tasks.md` group BB (sandbox + remote programme), item 763.
- **Owners:** WS02-P5-07 (storage and manifest column), WS02-P5-08 (handle, classes), WS02-P5-09 (refusal
  table), WS02-P5-10 (corpus), WS00-P1-13 (type stubs), route items WS11-P3-11 to P3-16, WS10-P3-02,
  WS08-P7-05 (see `tasks.md`).
- **Archive:** at the end of the wave that completes it (B6). It only ADDs requirements to `sandbox`; it
  MODIFIES none that another in-flight change modifies.
- **Schema:** none.

### Acceptance coverage (THE-792 to THE-796)

Bullets of the gates owned by this change are mapped below. Gate 2 and the export and sync bullets of gate 3
are covered by `add-private-workspaces`; gate 1 by `fix-truth-in-reporting`.

| Gate acceptance bullet                                                                                                        | Home                                                                                                                                |
| ----------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| THE-794: every row of the execution-route table goes through the handle or is a declared host-trust exception shown in doctor | "Every repo-influenced launch has a launch class"; "Untrusted launches are admitted only through one handle"                        |
| THE-794: no host spawn after a secure rejection on any route                                                                  | "A secure refusal never spawns on the host"                                                                                         |
| THE-794: hostile archive and symlink fixtures; failpoints; concurrent and repeated sync                                       | `add-private-workspaces`                                                                                                            |
| THE-795: adapter contract (enforced controls, immutable effective-policy identity, durable storage)                           | "Adapters report enforced controls and an effective-policy identity"                                                                |
| THE-795: every unsupported combination in a refusal table with a no-host-spawn test                                           | "Unqualified backends refuse secure policies"                                                                                       |
| THE-795: runtime or gateway loss yields `runtime-lost` with no host fallback                                                  | "A secure refusal never spawns on the host" (scenario: runtime loss); `add-private-workspaces` lifecycle                            |
| THE-795: canaries (synthetic home, sibling, git, cache and system paths; direct IP, alternate DNS, IPv6, proxy bypass)        | scenarios of "Unqualified backends refuse secure policies" and "Adapters report enforced controls and an effective-policy identity" |
| THE-795: presets versioned and pinned; resolution order; denies win; repo cannot widen                                        | "Untrusted launches are admitted only through one handle" (scenarios: repository overlay, named environment)                        |
| THE-795: fate of the cancelled THE-582 to THE-592 network stack                                                               | out of scope here; decided by WS04-P6-03 and recorded there                                                                         |
| THE-796: untrusted work defaults to private-copy, private home, offline, fail closed; interactive panes keep `standard`       | "Every repo-influenced launch has a launch class" (scenario: interactive default unchanged)                                         |
| THE-792: strict mode refusing every unsupported control; requested, resolved and enforced reported separately                 | "Adapters report enforced controls and an effective-policy identity"; `fix-truth-in-reporting`                                      |
