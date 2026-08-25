# Audit the sandbox across platforms; make its promises demandable

Linear: THE-47

## Why

thegn's sandbox is a family of per-platform mechanisms — Linux
bwrap/systemd-run/podman/docker (+ the in-flight gVisor/libkrun OCI-runtime
tiers) with a `[sandbox.limits]` systemd slice; macOS Apple `container` and
VM-mediated podman/docker; Windows kill-on-close Job Objects with OCI declined
by policy. What each mechanism actually enforces (filesystem, network, resource
ceilings, process scoping, honest escape class) exists only as predicates
scattered across `capabilities.rs`, `sandbox_backend.rs`, `sandbox_cpucap.rs`,
and `sandbox_support.rs` — there is no one place where a user (or a test) can
see the whole enforcement picture for a platform, and three honesty gaps
survive the recent sweeps:

1. **Windows Job Objects over-report.** `Backend::WinJobObject` is classified
   `SharedKernel` ("a container: namespaces + cgroups"), but a Job Object is
   process-tree lifecycle + resource scoping with **no** filesystem or network
   isolation — honestly a host process with scoping, not a container.
2. **The `HostProcess` escape note claims un-applied confinement.** It reads
   "only host-side LSM policy (Landlock/Seatbelt) confines it", but thegn never
   applies Landlock or Seatbelt anywhere — the note implies a mitigation that
   does not exist.
3. **Everything is fail-safe; nothing is demandable.** The chain degrades
   `podman → docker → bwrap → none` with a warning — right for interactive
   panes, but there is no way to say "this workload must not run below
   isolation class X, refuse instead". The only fail-closed point today is the
   VPN `on_error = "fail"`. Agent workloads (the queues' agent handoff) run on
   the bare host inside the resource slice, with no way to demand a boundary.

Separately, THE-47 asks the tier story (process-level → container → microVM) to
be scoped against candidate runtimes: kata-containers, microsandbox, youki,
agentbox, smolvm. The evaluation (design.md) concludes: youki adds no tier
(same shared-kernel class as runc, already reachable via `[sandbox]
oci_runtime` and correctly classified by the fall-through), kata is
containerd-shim-v2-shaped and not reachable from the podman/docker `--runtime`
seam, microsandbox is a server+SDK wrapper over the same libkrun the `krun`
runtime tier already reaches directly, and agentbox is a comparable
orchestrator product, not a runtime. The one genuine gap-filler is **smolvm**
(libkrun-family microVMs over KVM/HVF/WHP, OCI images, directory volumes, in
Rust) — the already-reserved `Backend::Smol` — which is the only candidate path
to a microVM tier on macOS and Windows, where `krun` (KVM-only) cannot go.

## What Changes

- **One enforcement matrix, derived, exhaustive by construction.** A pure
  `thegn-core` module declares, per `Backend` × `HostOs`, what is actually
  enforced: filesystem isolation, network isolation, resource-ceiling strength
  (hard / soft / none), process-tree scoping, and the honest `IsolationClass` —
  each cell **derived from the existing source-of-truth predicates**
  (`capabilities::from_parts_on`, `backend_suitable_on`, `CpuCap`,
  `sandbox_support`), never a second policy table. An exhaustive match makes
  adding a backend or OS without declaring its row a compile failure (the
  containment-label gate pattern). `thegn doctor` renders the current host's
  column, cell-honest: unverified backends (`mark-unverified-backends`) carry
  their caveat, degraded ceilings say "soft", and no cell claims a mitigation
  that was not applied.
- **An isolation floor, with a fail-closed option.** `[sandbox]
isolation_floor` (empty | `shared-kernel` | `userspace-kernel` |
  `guest-kernel`, reusing the `IsolationClass` vocabulary) demands that the
  resolved sandbox meet or exceed the named class, compared over the **honest**
  class (so macOS local OCI counts as guest-kernel; a Windows Job Object never
  counts as a container). `on_floor_miss = "degrade"` (default; warn + degraded
  flag, today's convention) or `"fail"` (refuse the launch — the VPN
  `on_error = "fail"` precedent). Repo overlays may only **raise** the floor
  (the clamp model); provider placements are out of floor scope and reported as
  `provider-managed`, never counted as a tier.
- **Windows honesty fix.** `WinJobObject` reclassifies to host-process-with-
  scoping; it satisfies no floor at `shared-kernel` or above. `WinAppContainer`
  (unshipped, AX 730) keeps a container-class reservation because it is an
  OS-enforced security boundary. The `HostProcess` escape note drops the
  Landlock/Seatbelt claim until thegn actually applies an LSM.
- **Agent-workload floors.** The shared agent-task engine (in-flight
  `add-agent-task-engine`) and the queues' agent handoff gain an opt-in to run
  under the resolved sandbox with the same floor semantics; a fail-closed floor
  miss on a queue task MUST surface as an **infrastructure failure**, never as
  a branch/agent failure (the merge-guard "never silently blame a good branch"
  doctrine).
- **smolvm scoped as the phased cross-platform microVM tier.** A verify-then-
  activate phase for `Backend::Smol`: liveness/exec verbs verified against a
  real smolvm install first (the `mark-unverified-backends` criterion — never
  guess verbs), path-preserving directory volume confirmed, and only then the
  matrix row claims `guest-kernel`. Until verified, the caveat stands and the
  row under-promises (`shared-kernel`), unchanged.

No new externally invokable operation: the matrix rides the existing `thegn
doctor` capability-catalog projection. A dedicated `thegn sandbox matrix` verb,
if ever added, needs its own `thegn_core::capability::CATALOG` row — explicitly
out of scope here.

## Impact

- **tasks.md**: AB 349–362 (container management — the sandbox core this
  audits), AC 363–372 / AD 373–384 (egress + observability gaps enumerated in
  the audit, not implemented here), AX 730–731 (Job Objects shipped; the
  reclassification + AppContainer reservation), AZ (macOS parity — the
  soft-ceiling and VM-mediated cells), AN 481–488 (the audit-log adjacency).
- **Affected specs**: `sandbox` (enforcement matrix, isolation floor, agent
  floors), `platform-windows` (Job Object honesty).
- **In-flight changes reconciled**: `add-oci-runtime-tiers` (the floor consumes
  its honest `userspace-kernel`/`guest-kernel` classes; this change adds the
  demand side on top — no overlap with the `oci_runtime` key itself);
  `verify-sandbox-mounts` (macOS honest classification + `from_parts_on(os)`
  idiom reused for the per-OS matrix); `mark-unverified-backends` (the matrix
  renders its caveat; the smolvm phase satisfies its `verified()` criterion);
  `add-windows-job-objects` / `add-windows-parity` (jobobject in the default
  chain before `host`, OCI declined, resource limits deferred — the matrix
  states all three; the reclassification composes with, not against, them);
  `add-sandbox-policy-engine` (orthogonal: policy is a finer gate _inside_ the
  boundary, the floor is a demand _on_ the boundary); `add-agent-task-engine`
  (owns the queue agent config shape — the floor keys land in its tables, so
  key naming defers to it).
- **Config**: `[sandbox] isolation_floor`, `[sandbox] on_floor_miss`, and the
  agent-task sandbox opt-in — each with a documented
  `config/config.toml.example` entry (spec'd here, written in implementation).
- **No DB schema change; no new render surface** (doctor is CLI; launch-path
  warnings reuse the existing degraded-flag + notification path).
