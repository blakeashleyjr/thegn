# Design

## Why a runtime modifier, not a new backend or VMM

gVisor (`runsc`) and libkrun (`krun`) are **OCI runtimes**: `podman/docker
--runtime <x>` runs an otherwise-normal container under them. So the stronger
isolation tiers are reached by a single flag on the _existing_ OCI backend, not a
new `Backend` variant (≈12 match arms + arg composition + teardown) and not a
from-scratch VMM subsystem. Crucially, because the container is still local and
thegn-owned:

- the worktree bind stays **path-preserving** (`-v /wt:/wt`), so host-side git
  and the compositor read the same tree — the load-bearing sandbox invariant;
- egress stays **`EgressKind::Enforce`** (our DNS filter / tgproxy run against a
  container we control), unlike a managed provider where policy is only
  _translated_.

A local-VMM provider (Firecracker/cloud-hypervisor as `Provider::LocalVm`) would
add CoW-fork / snapshot / suspend-resume by reusing the machine0 provider seam,
but at much higher cost (rootfs images, virtiofsd, vsock/ssh data plane). It is
deferred; the provider seam already exists for it.

## The one field, five seams

1. **Config** — `[sandbox] oci_runtime: String` (mirrors `oci_host`: global
   `[sandbox]`, not overlayed). Empty ⇒ daemon default.
2. **Spec** — `SandboxSpec.oci_runtime: Option<String>`, set in the resolver.
3. **Compose** — `oci_create_opts` emits `--runtime <x>` at _create_ for OCI
   backends only. podman/docker persist the runtime per-container, so
   `exec`/`inspect`/teardown (via `oci_prefix`) need no flag.
4. **Honest class** — `capabilities::isolation_for` maps, for OCI backends,
   `runsc → UserspaceKernel`, `krun → GuestKernel`, else the shared-kernel
   default. Placement short-circuits (`Provider`/`K8s`) and `egress`/`projection`
   are unchanged.
5. **Detection** — the pure `sandbox_runtime::decide` (unit-tested) decides
   keep-or-degrade from `(runtime, binary_present, kvm_present)`; the host probes
   `PATH` + `/dev/kvm` and applies it (clear the runtime + warn) before create,
   and `thegn doctor` reports availability.

## Decisions

- **Degrade, don't fail.** An unavailable runtime clears `oci_runtime` and warns
  (best-effort convention), so the pane still comes up on the default runtime.
- **Non-OCI backends ignore it.** bwrap/systemd/none have no `--runtime`; the
  flag and the class-raise are gated on `backend.is_oci()`.
- **Trust ladder.** `capabilities.rs` stays the source of truth the UI/doctor
  read; wiring a local guest-kernel sandbox into `trust_class` multi-host packing
  is a secondary follow-up, not required for single-host isolation.

## Risks

- Whether `podman exec` ever needs `--runtime` for a container created with one —
  assumed create-only (runtime is persisted); verify on the target podman.
- `krun` binary naming varies by distro (often a `crun` symlink); the probe looks
  for `krun` on `PATH` and degrades legibly if absent.
