# Design

## 1. The audit — what each platform actually enforces today

The matrix below is the audit deliverable, verified against the code
(`sandbox.rs` argv builders, `capabilities.rs::isolation_for`,
`sandbox_backend.rs::backend_suitable_on`, `sandbox_cpucap.rs`,
`sandbox_support.rs`) and the in-flight changes. It becomes a _derived,
build-gated artifact_ in phase 1 so it cannot rot into prose again.

### Linux (local placement)

| Backend                               | Filesystem                                                                                              | Network                                                                      | Resource ceiling                                                            | Process scoping                                                               | Honest class                                                      |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | --------------------------------------------------------------------------- | ----------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| podman / podman-rootful / docker      | bind worktree at real path; image rootfs; shared `.git/config` ro                                       | `network=none` / DNS filter / VPN sidecar — `EgressKind::Enforce`            | native `--cpus`/`--memory` + `thegn.slice` aggregate                        | engine-owned container lifetime                                               | `shared-kernel` (rootless adds the userns notch in `trust_class`) |
| + `oci_runtime = "runsc"` (in flight) | same (path-preserving bind kept)                                                                        | same                                                                         | same                                                                        | same                                                                          | `userspace-kernel`                                                |
| + `oci_runtime = "krun"` (in flight)  | same                                                                                                    | same                                                                         | same                                                                        | same                                                                          | `guest-kernel` (needs `/dev/kvm`)                                 |
| bwrap                                 | ro substrate binds, explicit mounts, tmpfs `/tmp`; `FileAccess::All` ⇒ `--dev-bind /` (no fs isolation) | `--unshare-net` only when `network = none`; otherwise host net — `Unmanaged` | `systemd-run --scope` CPUQuota via `CpuCap::ScopeHard`, degrading to `nice` | `--unshare-pid`, `--die-with-parent` (dropped for daemon panes; daemon reaps) | `shared-kernel`                                                   |
| systemd                               | `ProtectSystem`/`ProtectHome` unit props                                                                | `PrivateNetwork` only when `network = none`                                  | inline unit props + slice                                                   | `--collect` transient unit                                                    | `shared-kernel`                                                   |
| none                                  | none                                                                                                    | none — `Unmanaged`                                                           | slice/scope wrap (fail-safe)                                                | pgid only                                                                     | `host-process`                                                    |

Background jobs (fold gate, agent handoff) join `thegn.slice` via
`wrap_background_argv` — resource-bounded, **not** isolated.

### macOS (Apple silicon)

| Backend             | Notes                                                                                                                                                             | Honest class                                                                                                         |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| apple (`container`) | per-container lightweight VM; verbs live-verified                                                                                                                 | `guest-kernel`                                                                                                       |
| podman / docker     | run inside the podman-machine/colima VM; binds resolve **inside the VM** — `verify-sandbox-mounts` added in-container sentinel verification + share-root remedies | `guest-kernel` relative to the Mac (`verify-sandbox-mounts` fix), syscall boundary only — the VM has the worktree rw |
| bwrap / systemd     | impossible (`backend_suitable_on` rejects)                                                                                                                        | —                                                                                                                    |
| none                | no cgroup equivalent: CPU ceiling is **soft** (QoS/nice), doctor must say so                                                                                      | `host-process`                                                                                                       |

### Windows (native)

| Backend         | Notes                                                                                                                                               | Honest class                                                                     |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| jobobject       | kill-on-close Job Object: process-tree lifetime; resource limits **deferred** (add-windows-parity) until on-machine validation; no fs/net isolation | **today `shared-kernel` — over-report; this change → host-process-with-scoping** |
| appcontainer    | unshipped (AX 730 stretch); an OS-enforced capability/integrity boundary — the honest container-class reservation                                   | reserved                                                                         |
| podman / docker | **declined by policy** even when Desktop is installed: their Linux VM cannot bind the worktree at its real absolute path (add-windows-parity)       | —                                                                                |
| wsl             | reserved, no runtime behind it                                                                                                                      | —                                                                                |

### Findings (the gap list)

1. `WinJobObject` classified `SharedKernel` — over-promise (fix here).
2. `HostProcess` escape note names Landlock/Seatbelt, which thegn never
   applies — prose over-promise (fix here); actually applying an LSM to the
   `none` backend is future work, out of scope.
3. No demandable floor; the only fail-closed point is VPN `on_error = "fail"`
   (floor + `on_floor_miss` added here).
4. Agent handoff runs on the bare host (slice-capped only) — opt-in floor here.
5. No microVM tier on macOS/Windows: `krun` needs `/dev/kvm`; `Backend::Smol`
   is reserved-unverified (phased activation here).
6. Resource ceilings: hard only on Linux-with-delegation; macOS soft; Windows
   deferred — the matrix must render the strength, not just presence.
7. `Backend::Smol` classified `shared-kernel` — an _under_-promise once
   verified (smolvm is a microVM); acceptable while unverified, corrected by
   the activation phase.
8. Enumerated, not implemented here: AC 365/366 (DNS proxy, single auditable
   egress), AD 376–384 (shell/process/fs audit trails) — the observability
   half of a full audit story; left to their roadmap rows.

## 2. The matrix module

`thegn_core::sandbox_matrix` — pure, unit-tested to the 95% gate:

- `row(backend: Backend, os: HostOs) -> EnforcementRow { fs, net, ceiling,
scoping, class }`, each field **derived** by calling the existing predicates
  (`Capabilities::from_parts_on`, `CpuCap`-shape rules, profile family) — the
  `capabilities.rs` doctrine: aggregation, never a second policy table.
- Exhaustive `match` over `Backend` × `HostOs` (the containment-label gate
  pattern): a new variant fails compilation until its row is declared.
- `thegn doctor` renders the host's column next to the existing
  `sandbox_support` rows; unverified backends carry the
  `mark-unverified-backends` caveat in their row; ceiling cells render
  hard/soft/none from the probed `CpuCap`, mirroring its `label()` honesty.

Doctor is a CLI command (no event-loop or render-plan impact); the matrix is
computed on demand, no background work, no DB.

## 3. The isolation floor

- **Vocabulary**: reuse `IsolationClass::as_str` names. Floor values:
  `shared-kernel`, `userspace-kernel`, `guest-kernel`; empty = no floor
  (default, today's behavior). `host-process` as a floor is meaningless
  (always met) and rejected by config validation.
- **Comparison** is over the honest class of the _resolved_ spec
  (`Capabilities::derive`), after backend-chain selection and the
  `sandbox_runtime` degrade decision — so a `krun` that degraded to `crun` is
  compared as `shared-kernel`, and a macOS local OCI compares as
  `guest-kernel`. Ordering: host-process < shared-kernel < userspace-kernel <
  guest-kernel. `ProviderManaged` is **outside the order**: an explicit
  provider placement bypasses the floor (the user chose to trust the provider;
  co-tenancy trust stays `trust_class`'s job — no second ladder) and is
  reported as `provider-managed`.
- **Miss policy**: `on_floor_miss = "degrade"` (warn + degraded flag +
  deduped notification — the existing fallback-reporting path) | `"fail"`
  (the pane does not launch; the error names the floor, the best class
  available, and the remedy — install runsc/krun, start the runtime). The
  dormant-runtime prompt composes naturally: starting the runtime and
  re-resolving can satisfy the floor.
- **Trust clamp**: the repo overlay may only raise the floor and may only
  harden the miss policy (`degrade → fail`); lowering either is denied and
  surfaced, exactly like the existing `network` clamp. Profile/env overlays
  are trusted layers and apply unclamped.
- **Fail-closed placement rule**: with `on_floor_miss = "fail"`, a floor miss
  MUST abort before any process spawns on the host — never "launch on the
  host while we report the miss" (the tunnel-failure precedent).

## 4. Agent-workload floors

The queues' agent handoff and the fold gate currently run host-side inside
`thegn.slice` (`wrap_background_argv` — fail-safe by design: a cap that breaks
the gate silently blames a good branch). This change keeps the resource wrap
fail-safe and adds an **explicit, opt-in** boundary:

- The agent-task configuration (shape owned by in-flight
  `add-agent-task-engine`; key naming defers to it) gains a sandbox opt-in +
  per-task `isolation_floor`.
- Enforcement: the task's argv is wrapped by `sandbox::enter_argv` for the
  resolved spec, floor-checked as §3.
- **Failure attribution is the load-bearing rule**: a fail-closed floor miss
  (or sandbox setup failure) on a queue task MUST be reported as an
  _infrastructure_ failure — the queue entry is retried/held, never marked as
  a failed branch. This is the merge-guard doctrine applied to isolation: a
  broken boundary must be loud, and must not convict the code under test.
- Default posture stays host+slice (changing it would break configured agents
  that need host credentials/keychains); the recommended agent default —
  documented in the example config — is `shared-kernel` floor with `fail` on
  Linux, `degrade` elsewhere.

## 5. Candidate runtimes (README-level evaluation)

| Candidate           | What it is                                                                                                                                                               | Verdict for thegn                                                                                                                                                                                                                                                                               |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **youki**           | OCI runtime in Rust; drop-in runc alternative (`--runtime youki`); same namespaces/cgroups isolation                                                                     | **No tier.** Same `shared-kernel` class; already reachable via `[sandbox] oci_runtime = "youki"` and correctly classified by the fall-through arm. Nothing to build.                                                                                                                            |
| **kata-containers** | Lightweight-VM container runtime; integrates as a **containerd shim v2** (CRI-O/containerd), multi-hypervisor                                                            | **Not reachable from our seam.** thegn drives podman/docker `--runtime <binary>`; kata's shim-v2 shape needs a containerd-based engine thegn doesn't have. Same class (`guest-kernel`) is already reachable via `krun` on Linux. Keep unlisted (not even reserved) until an engine seam exists. |
| **microsandbox**    | Beta server (`msb`) + SDKs + MCP over libkrun microVMs; OCI images; Linux KVM / macOS HVF / Windows WHP                                                                  | **Ruled out as a backend**: the `krun` OCI-runtime path reaches the same libkrun class with no server, no SDK, and our existing teardown/compose seams. Its MCP surface is an integration idea, not a sandbox.                                                                                  |
| **smolvm**          | Rust CLI microVMs (libkrun/libkrunfw) on KVM / Hypervisor.framework / WHP; boots OCI images; directory `--volume` binds; sub-second cold start; `.smolmachine` artifacts | **The gap-filler**: the only candidate microVM tier for macOS and Windows, where `krun` (KVM-only) cannot go — and `Backend::Smol` is already reserved for it. Phased verify-then-activate (§6).                                                                                                |
| **agentbox**        | TypeScript orchestrator: Docker + FUSE overlays, cloud providers, per-agent browsers/IDEs                                                                                | **Comparable product, not a runtime.** Its git-credential-stays-local and checkpoint ideas map to existing roadmap rows (AE 385/390); nothing to adopt as a backend.                                                                                                                            |

## 6. smolvm activation (phased, honesty-gated)

`Backend::Smol` today: parses, sits in the OCI family, `liveness_argv` returns
`None` (PATH probe only), `verified()` says no (per `mark-unverified-backends`
— the candid "guessing verbs regressed the Apple backend three times" rule).
Activation is therefore gated on a real-install verification pass:

1. Verify the actual CLI surface against a live smolvm (create/exec/stop/rm
   equivalents, `--volume <wt>:<wt>` path-preserving directory bind, exit-code
   and stderr shapes) on at least Linux + macOS. **No code lands before this.**
2. Only then: liveness argv, enter/teardown wiring through the existing
   OCI-family arms (or a `Smol` family if its verbs diverge from the
   docker-clone assumption), matrix row flips to `guest-kernel`, caveat drops.
3. Windows (WHP) rides the same row later; it would be the first _isolating_
   Windows backend and slots into the chain before `jobobject`.
4. If verification finds the bind is not path-preserving (single-file mounts
   are already documented as unsupported), the backend stays reserved — the
   bind-at-real-path invariant is load-bearing and not negotiable.

## Security

**Threat model per tier — what each does and does not contain:**

- `host-process` (+ scoping): contains _runaway resources and orphans_ only
  (slice ceilings, pgid/Job-Object kill-tree). No confidentiality or integrity
  boundary: full host fs/net/secrets. Right for trusted interactive work.
- `shared-kernel` (bwrap/systemd/OCI): contains fs access (mount namespace;
  shared `.git/config` ro), optionally network (`none`/DNS filter/VPN-only
  egress), resources, pids. Does **not** contain a kernel LPE: any allowed
  syscall path is host-kernel attack surface. Secrets: pane env rides the
  wrapper (AB 754 tracks getting `env_overrides` off /proc-visible argv).
  Rootless adds the userns notch (escape lands unprivileged).
- `userspace-kernel` (runsc): contains the syscall ABI behind the Sentry;
  escape needs a Sentry or host-allowlist bug. Worktree is still bind-mounted
  rw — _data_ exfiltration protection comes from egress policy, not the class.
- `guest-kernel` (krun / apple / macOS VM-mediated OCI / smolvm-when-verified):
  contains the kernel boundary; escape needs a VMM/KVM/HVF bug. Same caveat:
  the guest still holds the worktree rw and whatever env it was given — the
  class bounds _host_ compromise, not repo-data misuse. Egress policy and the
  policy engine remain the controls for that.
- `provider-managed`: no verifiable boundary — trust in the provider's TCB and
  operator; never counted as a local tier.

**Fail-safe vs fail-closed — when each is right:**

- _Fail-safe (degrade + warn)_ is right when the promise being missed is
  **availability-adjacent** and the user is present to see the warning:
  interactive backend chain, resource caps (a cap that breaks the gate blames
  a good branch), runtime degrade (`runsc`/`krun` → default), personal-layer
  provisioning. The non-negotiable companion is honesty: every degrade is
  labelled, flagged, and warned — already spec'd, and extended by the matrix.
- _Fail-closed (refuse)_ is right when the promise is **the security
  boundary itself** and nobody is watching, or the user said so explicitly:
  VPN `on_error = "fail"` (exists), `sealed` refusing a VPN (exists), digest
  mismatch refusing boot (exists), and now `on_floor_miss = "fail"` and the
  queue floor. Rule of thumb encoded here: _interactive defaults fail safe;
  unattended/agent workloads and explicit demands fail closed._
- Failure attribution: fail-closed misses on queue tasks are infrastructure
  failures, never branch verdicts (§4).

**Agent-workload defaults**: recommended (and example-config-documented)
posture for agent tasks is sandbox on where a runtime exists, floor
`shared-kernel`, `fail` on Linux / `degrade` with warning on macOS/Windows
until their isolating backends ship. The shell keeps working with no agent and
no sandbox configured — everything here is additive.

**Credential handling**: no new secrets, no new config that holds tokens; the
floor keys are plain enums. **New write surface**: none — the floor only
narrows what may launch; the matrix is read-only reporting. **Blast radius**:
a wrong matrix cell is a reporting bug (no enforcement flows _from_ the
matrix — cells are derived from the enforcing predicates, so a lie requires
the enforcement source itself to be wrong); a wrong floor comparison can
either over-block (annoying, visible) or under-block (launch below floor) —
the comparison is pure core logic under the 95% gate with per-OS pinned tests
(`from_parts_on` idiom).

## Alternatives considered

- **A hand-written enforcement table** (docs or a static const): rots
  immediately and can disagree with the resolver — rejected for the derived
  exhaustive-match design.
- **Floor as a backend allowlist** (`backend_chain` already exists): chains
  express _preference_, not _demand_, and cannot see the runtime-raise
  (`runsc` on podman) or the per-OS class shift; the class comparison can.
- **Making agent sandboxing default-on**: breaks every configured agent that
  reads host credentials; opt-in with a documented recommended posture instead.
- **Adopting kata/microsandbox now**: both reach classes we already reach (or
  can't be driven from our seam); rejected at README level, revisitable if a
  containerd-engine seam ever lands.

## Open questions

1. Should `on_floor_miss = "fail"` also gate **daemon-owned reattach** (a
   session that was compliant at spawn but whose runtime died)? Proposed: no —
   reattach reports the observed containment truthfully; re-resolution applies
   the floor on the next spawn.
2. Floor semantics for `Placement::Ssh` (remote Linux host): the class is
   computed for the remote host's backend, but the probe data is thinner.
   Proposed: compare over what the remote probe proves, and let `Unreachable`
   count as a miss.
3. Does the matrix belong in the F1 help corpus as a generated page (like the
   keybindings page)? Deferred; doctor is the first surface, and the help
   ratchet only binds pages that claim actions.
