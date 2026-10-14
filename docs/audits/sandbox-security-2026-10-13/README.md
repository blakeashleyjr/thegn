# Sandbox security audit and isolated-worktree design

Audit date: 2026-10-13. Source revision:
`38e09ded78652c6cf56b0e3a7b6a7c16d18dcd95` (2026-10-12).
Scope: audit and design only. No production code, configuration defaults or
public APIs were changed. Evidence fixtures are outside application paths.

## Decision

The current default is a convenience sandbox, not sufficient confinement for
arbitrary untrusted execution. There are also paths where a stronger requested
policy is bypassed or loses enforcement. Recommend **a durable independent
private Git repository per workspace, private home, offline by default, and
mandatory final policy admission across every launcher**. Return only explicitly
reviewed file changes and a patch; keep sandbox history private.

Thegn should own workspace durability, review, conflict detection and recovery,
with pluggable qualified execution runtimes. OpenShell is a promising external
policy adapter; it still needs local conformance testing and Thegn-owned sync.
Keep linked Git worktree and existing live mount as explicit selectable modes
whose shared-file/Git exposure is visible. See the detailed
[target design and migration](design.md) and
[external-runtime comparison](external-runtime-fit.md).

## Evidence and severity

High means a workload can receive substantially broader filesystem, credential,
network or execution authority than an untrusted-workflow expectation or a
specific requested policy. Medium means a narrower control/visibility gap or
an operational defect that can undermine admission. This report assumes an
untrusted repo/agent can issue arbitrary commands within its admitted execution
environment; it does not claim a container/kernel escape. Some defaults are
documented deliberate behavior, while other findings bypass an explicit policy.

Evidence labels: **S** = source trace; **H** = extracted production-helper probe;
**R** = compiled Thegn-core/runtime probe; **G** = synthetic Git experiment.
Runtime records remain authoritative for which platforms actually ran.
No source-only finding is represented as a tested platform escape.
Artifact hashes and selected source hashes are retained in
[artifact-manifest.json](artifact-manifest.json); documentation/fixture syntax
and local-link checks are recorded in [verification.json](verification.json).

| ID / severity | Finding, affected paths and impact                                                                                                                                                                                                   | Evidence / reproduction                                                                                                                                                                                                                                                                                                                                                                                              |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| F01 High      | Checked-in `env` can select an existing weaker user environment after repo policy clamping, undoing global sealed/none/fail policy. Interactive agents/tools and environment consumers are affected when no explicit selection wins. | **S/R**, [L1](launcher-evidence.md#l1--high-repository-environment-selection-can-undo-the-sandbox-clamp). Synthetic `env=hostdev` changes sealed/none/shared-kernel/fail to none/open/host/off/degrade. Requires that weaker named environment to exist.                                                                                                                                                             |
| F02 High      | Tests, executable discovery, merge gates and regeneration run project code on the host with inherited environment, outside the pane sandbox. Resource caps do not revoke host authority.                                             | **S**, [L2](launcher-evidence.md#l2--high-local-testbuildgate-execution-remains-outside-pane-sandbox-policy). `task.rs:196`, `integrate_gate.rs:874`, `integrate.rs:651`; synthetic canary recipe retained, full UI launcher runtime not exercised.                                                                                                                                                                  |
| F03 High      | Unattended queue agents default to credentialed host execution; sandbox opt-in resolves raw global sandbox config and ignores effective repo/environment policy and parts of floor intent.                                           | **S**, [L3](launcher-evidence.md#l3--high-queue-agents-default-to-credentialed-host-execution-and-opt-in-resolution-drops-per-repo-policy). `agent_run.rs:193,203`, queue defaults/call sites. Workflow enablement is user-owned, but is not a confinement decision.                                                                                                                                                 |
| F04 High      | Optional standalone terminal sandbox requests can become host shells without final floor admission; one fallback emits no degradation warning.                                                                                       | **S**, [L4](launcher-evidence.md#l4--high-secure-terminal-requests-can-fall-back-to-a-host-without-final-floor-admission). `panes.rs:215,269,329`; sealed home refusals already exist. Full terminal spawning not tested.                                                                                                                                                                                            |
| F05 High      | Retained OCI reuse checks running/home marker and mount source sets, omitting network, mount modes/destinations, image, runtime, caps, env and limits. Narrowed requests can enter a broader old container.                          | **S/H/R**, [B01](backend-evidence.md#b01--high-persistent-container-reuse-does-not-reconcile-the-security-policy). Helper accepts NAT→none/rw→ro; actual container transition and its limitations are recorded in [runtime evidence](runtime-evidence.md).                                                                                                                                                           |
| F06 High      | Hostname rules filter DNS rather than packets. Direct IP/alternate DNS remain possible; bwrap/systemd/provider/remote paths do not enforce the local OCI filter. DNS setup failure can omit filtering.                               | **S/R**, [B02](backend-evidence.md#b02--high-hostname-settings-are-dns-filtering-with-silent-unsupported-paths-and-no-packet-boundary). Actual bwrap deny-all still connects to local TCP/UDP responders. The OCI DNS argument's acceptance is recorded separately from data-plane bypass.                                                                                                                           |
| F07 High      | Process-wide DNS singleton retains the first policy and upstream for later workspaces, ignoring a later deny-all request.                                                                                                            | **S/H/R**, [B03](backend-evidence.md#b03--high-dns-singleton-retains-the-first-policy-for-every-later-sandbox). Local upstream: first allowlist then deny-all returns same port and rcode 0 for first-policy name.                                                                                                                                                                                                   |
| F08 High      | Hardened keeps ambient home readable on applicable Linux toolchain paths, forwards token env names, and shares writable caches/tool state. Read-only home is not credential confidentiality.                                         | **S/R**, [B04](backend-evidence.md#b04--high-default-hardened-exposes-ambient-home-credentials-and-writable-host-execution-state). Synthetic credential/sibling reads succeed under hardened bwrap. GPG's configured shorthand is malformed (F17), so distinguish intended grant from actual mount. Sealed hides home but still forwards configured token env, including the synthetic GH_TOKEN observed at runtime. |
| F09 High      | Live/linked worktree mounts expose writable common Git metadata: sandbox refs, objects, hooks and sibling-worktree records are host-shared even under sealed/autopilot.                                                              | **S/R/G**, [B05](backend-evidence.md#b05--high-live-worktree-mounts-grant-writable-shared-git-metadata). Shared-Git canary write; copied linked directory retains source common dir; independent Git experiment isolates private refs.                                                                                                                                                                               |
| F10 High      | Default enabled/auto/hardened can end at host because floor is off and local chain names host. “Sandbox enabled” is not minimum confinement.                                                                                         | **S/R** at resolver/floor level, [B06](backend-evidence.md#b06--high-enabled-sandbox-does-not-require-a-boundary-by-default). Missing backend/core fallback and off/degrade/fail decisions tested; explicit interactive and sealed paths now have stronger refusal (below).                                                                                                                                          |
| F11 High      | Provider-native panes bypass local sandbox admission and forward configured credentials without proving filesystem/egress controls; ProviderManaged bypasses ranked floors.                                                          | **S**, [L5](launcher-evidence.md#l5--high-provider-native-execution-forwards-credentials-without-proving-configured-egresscontainment-controls). `agent.rs:1638`, `sandbox_floor.rs:94`. Provider-side enforcement may exist; no provider canary run.                                                                                                                                                                |
| F12 High      | Native Windows reserved AppContainer is marked available/shared-kernel while generated argv remains a plain shell and no AppContainer token implementation is present.                                                               | **S**, [B07](backend-evidence.md#b07--high-reserved-native-windows-appcontainer-is-reported-as-available-confinement). Availability/capability/truth source trace; Windows runtime unavailable. Job Object process grouping is not filesystem/network confinement.                                                                                                                                                   |
| F13 High      | VPN sidecar/proxy mode does not establish a mandatory tunnel-only datapath. Proxy env can be ignored; default sidecar routes and optional exit-node selection are not packet denial.                                                 | **S**, [B11](backend-evidence.md#b11--high-vpn-sidecar-attachment-does-not-establish-a-mandatory-tunnel-only-datapath). Runtime/provider image may add independent controls; no VPN credentials or external tunnel used.                                                                                                                                                                                             |
| F18 High      | Host Git status/mutations can run mutable executable configuration and hooks outside lifecycle trust gates. Writable common hooks provide a path from a confined agent to later host execution.                                      | **S/G**, [L6](launcher-evidence.md#l6--high-host-git-can-execute-mutable-repository-helpers-and-hooks), [Git canaries](host-git-canary.json). Production status argv executes synthetic fsmonitor; commit `--no-verify` still executes post-commit. Direct Git primitive tests, not compiled Thegn UI tests.                                                                                                         |
| F14 Medium    | Podman audit filters legacy `io.thegn=true`, but new OCI containers carry `thegn.managed=true`; DNS ring is not wired to durable per-workspace connection audit.                                                                     | **S**, [B08](backend-evidence.md#b08--medium-audit-subscription-does-not-match-new-container-ownership-labels). Label creation/subscription mismatch; bounded DNS ring has no workspace attribution.                                                                                                                                                                                                                 |
| F15 Medium    | Block-only DNS names compare case-sensitively, allowing mixed-case queries.                                                                                                                                                          | **S/H**, [B09](backend-evidence.md#b09--medium-block-only-dns-matching-is-case-sensitive). `blocked.test` denied, `BLOCKED.TEST` allowed; separate from the stronger DNS bypasses.                                                                                                                                                                                                                                   |
| F16 Medium    | Systemd sees ambient host root; ProtectSystem=yes and ProtectHome do not enforce a worktree-only filesystem scope.                                                                                                                   | **S/R**, [B10](backend-evidence.md#b10--medium-systemd-file_access-is-partial-hardening-rather-than-filesystem-scope). Actual hardened and sealed systemd read/write a synthetic host `/dev/shm` canary outside HOME; bwrap/OCI do not see it. No privilege escalation claimed.                                                                                                                                      |
| F17 Medium    | Default `~/.gnupg:rw` is parsed as destination `rw`, not a same-path mount mode; `host:dest:rw` is also unsupported and can be silently omitted. This exposes a writable GPG alias or refuses runtime startup.                       | **S/R**, `sandbox_mounts.rs:585`, [runtime evidence](runtime-evidence.md). Literal-default bwrap reads/writes the synthetic credential through `/rw`, while its original home path stays read-only; systemd refuses relative `rw`. Supported writable grammar is `host` or `host:dest`.                                                                                                                              |
| F19 Medium    | Default-enabled local language servers run on the host outside pane policy. Their environment is filtered, but any project tooling/plugins they execute retain host filesystem/network authority.                                    | **S**, [L7](launcher-evidence.md#l7--medium-language-servers-receive-unconfined-host-filesystemnetwork-authority). Server/version behavior is conditional; no LSP workload or exploit was executed.                                                                                                                                                                                                                  |

Additional host execution and trust coverage, including Git executable config,
MCP/plugin commands and language-server launchers, is recorded in
[launcher evidence](launcher-evidence.md). These paths must be part of the
target admission inventory; approving a tool binary is not automatic trust in
every project's code it processes.

## What already protects users

The plan's initial tracing was directionally useful but partly stale.
Interactive worktree launch now refuses unavailable explicit containment,
exhausted sealed confinement and many non-local failures. Final host fallback
checks the requested floor; a local auto chain still naming host remains a
deliberate weaker default. Sealed denies ambient credential mounts, creates a
private home and refuses backends that cannot hide it. These protections do
not make every launcher obey the same policy, or reconcile old container
network state.

Repository sandbox overlays have an exhaustive trust classifier: deny backend,
remote routing and env authority; clamp hardening/network/floors; gate additive
mount/image/lifecycle requests. Empty repo hostname allowlists become DNS
deny-all, so the resulting config is narrower syntactically but is not packet
denial. The separate environment-name route bypasses this narrowing (F01).

Repo hooks and devcontainer lifecycle categories require canonical approvals;
devcontainer initialize explicitly requests host execution. Global/workspace
hooks are user-owned. The raw devcontainer CLI is limited to approved snapshots
and a permissive policy subset, refusing controls it cannot honor. Host trust
must explicitly cover command dependencies that remain mutable.

Autopilot forces sealed/network-none, clears credentials/mount additions,
rejects several unsupported backends and preflights before dispatch. It still
uses writable shared common Git metadata and requires actual network-none
support from the selected backend. These positive controls are detailed in
[launcher evidence](launcher-evidence.md).

## Requested versus observed enforcement

The full backend/platform mechanism matrix is in
[backend evidence](backend-evidence.md#requested-controls-and-actual-backend-mechanisms).
The [design network matrix](design.md#network-presets-and-policy-ownership)
identifies which endpoint presets currently require refusal.

| Execution route                                        | Effective-policy path today                                                | Principal residual                                                                   |
| ------------------------------------------------------ | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| Worktree shells, configured agents/tools, daemon opens | Shared prepare/admit/ensure/preflight path, then final command composition | Environment-name override, reuse, post-ensure contributors, shared data/credentials. |
| Standalone terminal tabs                               | Separate optional wrap and home gate                                       | Final floor admission absent on fallback.                                            |
| Queue/CI/review/merge agents                           | Queue sandbox switch; opt-in raw global spec and queue floor               | Default host, effective repo/environment policy omitted.                             |
| Autopilot                                              | Forced sealed and credential-free spec, ensure/preflight                   | Shared Git; per-backend actual enforcement.                                          |
| Tests/discovery/gates/regeneration                     | Host resource-capped commands                                              | Project code outside pane policy.                                                    |
| Host lifecycle hooks/devcontainer initialize           | User trust/approval, filtered env, process ownership                       | Full host authority; approval scope must be explicit.                                |
| Container devcontainer lifecycle                       | Approved execution subset through provision/init                           | Builds and lifecycle authority must remain inside effective runtime contract.        |
| Provider-native                                        | Native API/transport plus credential forwarding                            | No evidence of Thegn network/filesystem admission.                                   |
| Host Git reads/mutations                               | Local Git helpers, with some narrowly sanitized integration paths          | Executable config and common hooks outside pane policy.                              |
| Language services                                      | Host process with curated env and resource limits                          | Project-dependent executable behavior outside pane policy.                           |
| MCP/plugins/editors/viewer or arbitrary-command panes  | User-installed/configured host extension or separate PTY helper            | Tool exposure/quoted filenames are not OS confinement; explicit host trust needed.   |

## Validation and limits

See [validation.md](validation.md) for commands, outcomes and future acceptance
scenarios; [runtime-evidence.md](runtime-evidence.md) for actual Linux backend
results and logs; [private-copy-results.json](private-copy-results.json) for 18
synthetic Git/design checks. The private-copy fixture demonstrates independent
Git state, file-only binary patch transfer, ignored-file behavior, baseline
conflict detection, process replacement and recovery predicates. It does not
implement or prove crash-safe production sync, runtime restart storage or a
hostile-path exporter.

Runtime probes use synthetic HOME/repositories/credential canaries, unique
audit-owned resources, cached images and controlled local TCP/UDP endpoints.
No real credentials or external test services are used. A backend CLI present
on PATH is not a passed security test. Unavailable/blocked macOS, Windows,
rootful OCI, alternate runtimes, VPN and provider combinations remain explicitly
unverified. Focused tests and any failures/skips are disclosed; no full-suite
or all-launcher certification is claimed.

## Prioritized remediation and migration

Before describing an untrusted workflow as secure, close policy-selection and
host-launcher bypasses, refuse unsupported controls, and compare complete live
container policy before reuse. Correct malformed mount defaults and false
Windows capability labels. Repair audit attribution. Existing home/cache/Git
exposures then need an independent workspace and explicit credential/cache
grants, rather than stronger names for the same live mounts.

Implement durable capture/storage before execution integration; add quarantined
file/patch review and journaled conflict-refusing sync; qualify native offline
and external endpoint-policy adapters; then default new untrusted work to
private copy. Preserve existing dirty workspaces and offer explicit conversion.
Do not automatically move files, import sandbox refs, widen endpoint presets
or discard private copies. Details and release gates are in [design.md](design.md).
