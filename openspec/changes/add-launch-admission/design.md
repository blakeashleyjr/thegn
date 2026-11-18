# Design

Sources: `docs/audits/sandbox-security-2026-10-13/design.md` ("Runtime/launcher contract and audit",
"Network presets and policy ownership"), THE-794 and THE-795, and the THE-791 alignment decisions T1 to T3, T7,
T8 and T11.

No render damage channel, event-loop wake path or SQLite schema is touched. Admission is a pure decision in
`thegn-core` plus a host function that mints the handle; nothing blocks the event loop and nothing runs before
the first frame. No `user_version` bump.

## Launch classes

Every spawn of repo-influenced code has one class, defined once in `thegn_core::launch_class` (stub
WS00-P1-13; semantics WS02-P5-08):

- `InteractiveOwner`: a pane the user opens from the TUI or attaches to (worktree shells, interactive agent
  panes, terminal tabs, configured tools started interactively).
- `Untrusted(kind)`: `QueueAgent`, `Autopilot`, `AgentRun`, `MergeGate`, `Regenerate`, `TestRun`,
  `TestDiscovery`, `RepoLifecycle`, `Lsp`, `ProviderExec`, `HeadlessTool`, `Plugin`.
- `HostTrust(kind)`: `UserHook`, `HostGit`, `McpServer`, `TrustedPlugin`, `Editor`, `Viewer`. These are the
  declared host-trust exceptions: listed in `thegn doctor` and scoped to a command digest.

The class is orthogonal to `LaunchKind { Interactive, Batch }`. `LAUNCH_ROUTES` is re-keyed by class.
Defaults per class (the table is the contract that the gate-5 flip implements): `InteractiveOwner` stays
`standard`, `git-worktree`, direct network, as today; untrusted agent kinds get `sealed`, `private-copy`, a
private home, the `agent` network preset and no ambient credentials; untrusted batch kinds get the same with
the `offline` preset; a miss refuses and never runs on the host. A repository can narrow a class default but
never widen it.

## One handle

`thegn_core::admission::evaluate(&AdmissionRequest, &EnforcedControls) -> AdmissionVerdict` is pure.
`AdmissionRequest` carries the class, source trust, the sha256 digest of the resolved policy ceiling
(`ceiling_digest: [u8; 32]`, so the core type does not need the host's resolved-policy type), an optional
workspace reference and the command digest. The host function `sandbox_admission::admit_launch(req, truth)`
returns `Result<LaunchAdmission, Refusal>`. `LaunchAdmission` has private fields (`class`, `policy_id`,
`controls`, `generation`) and is the only input the spawn helpers of untrusted classes accept, so a refused
request cannot reach a spawn by construction. It validates mount source, destination and mode, home view,
environment channels, network policy hash, capabilities, runtime implementation, limits and process scope
against the enforced controls. It extends today's `Admission`; the interactive-pane and devcontainer routes
migrate to it.

## Adapters report, they do not claim

An adapter is any backend or provider. `EnforcedControls` is computed: the `Truth` overlay of the enforcement
manifest, the host report (confinement, Seatbelt or Windows probe results) and, for non-built-ins, a live
attestation with a 30-day TTL. Providers supply an egress report and a provider verification. The effective
policy identity `EffectivePolicyId` is a sha256 over the policy fingerprint of the resolved spec, the egress
policy digest, the workspace generation, the backend definition digest and binary hash, and the manifest
revision. It is stamped as the `thegn.policy` label, carried by the handle and written to the audit record.
Reuse requires equality; a changed static control recreates compute without deleting workspace data. Durable
storage is a manifest column: private-copy admission refuses ephemeral storage.

## Secure policies and the refusal table

A "secure policy" is any untrusted class or a `sealed` profile. A backend satisfies it only when its enforced
controls include a private filesystem scope, a hidden or denied home, own-namespace or VM network isolation
(or verified Seatbelt or WFP deny), and durable private storage for private-copy, and its verification is
`Verified` or `Attested` on that OS. The table below is the requirement; each row has a no-host-spawn test
(WS02-P5-09).

| OS      | Qualifies (when)                                                                                                  | Refused for secure policies (until)                                                                             |
| ------- | ----------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| Linux   | bwrap with the confinement helper; OCI rootless or rootful, fresh or attested; microVM backends after attestation | systemd (permanently, no filesystem boundary without a user namespace); Landlock only (no private `/tmp`); host |
| macOS   | OCI in a VM engine after per-engine evidence; Seatbelt only after verification                                    | Seatbelt before verification; Apple `container` until evidence; host                                            |
| Windows | WSL with an inner Linux backend after seal evidence; MXC after device evidence (offline only)                     | native Job Object (always); MXC and WSL before their evidence; host                                             |

## Fail closed

A secure request never becomes a host spawn after probing, image preparation, runtime loss or exec failure.
An unavailable enforcement or audit mechanism is visible and, where it is part of the requested policy,
rejects the launch. Records compare requested, resolved and observed properties, name the enforcing backend
separately from the request, carry workspace, run and runtime ids and the policy generation, and never contain
credential values or secret-bearing argv.

## Alternatives rejected

- A second handle type beside `Admission`: two types invite a route that checks neither.
- Trusting a backend's declared isolation class or an environment name as proof of controls.
- Degrading an invalid or unsupported rule to an empty allowlist or to unrestricted networking.
