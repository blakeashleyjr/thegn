# Add selectable OCI runtime tiers (gVisor / libkrun)

## Summary

Add a single `[sandbox] oci_runtime` config key that runs a worktree's OCI
container under a stronger OCI runtime and reports the **honest** isolation class
it yields — reaching two isolation tiers above today's shared-kernel default
**without a new backend or VMM subsystem**:

- `oci_runtime = "runsc"` → **gVisor**: a userspace application kernel services
  the container's syscalls (no `/dev/kvm`). Isolation class `userspace-kernel`.
- `oci_runtime = "krun"` → **libkrun**: the container boots inside a
  hardware-virtualized microVM (needs `/dev/kvm`). Isolation class `guest-kernel`.
- unset / `runc` / `crun` → today's shared-kernel container (unchanged).

Both gVisor and libkrun register as **OCI runtimes**, so this is a `--runtime
<value>` flag on the existing Podman/Docker backend at container *create*. Every
existing seam is reused: the worktree bind stays **path-preserving** (host-side
git keeps working), egress stays **thegn-enforced** (our DNS filter runs on a
container we own — unlike a remote provider), and detection/teardown/compose are
untouched. When the requested runtime isn't installed (missing binary, or no
`/dev/kvm` for `krun`), the sandbox **degrades to the daemon default with a
visible warning** rather than failing the pane.

This ships the `IsolationClass::UserspaceKernel` / `GuestKernel` and
`TrustClass::T3GuestKernel` vocabulary that the capabilities/trust modules
already model but nothing but `Backend::Apple` reached.

Phased: **runsc first** (validates the whole path with no KVM), **krun second**
(guest kernel, `/dev/kvm`). A local-VMM `Provider::LocalVm` (Firecracker for
CoW-fork / snapshot / suspend-resume, reusing the machine0 provider seam) is
explicitly **out of scope** here and deferred to a later change.

## Impact

- tasks.md: **AB** (container management, AB 349–362) — the worktree-sandbox core
  this extends — and the isolation-class work in `capabilities`/`trust_class`.
  Complements, and is orthogonal to, the managed-provider lineage (**AE**,
  machine0/VPS): those are *placements* we don't control; this is a stronger
  boundary on a container we *do* control.
- Code: `[sandbox] oci_runtime` (config + example), `SandboxSpec.oci_runtime`,
  `--runtime` injection in `oci_create_opts`, the runtime→class map in
  `capabilities::isolation_for`, the pure availability rule in a new
  `sandbox_runtime` module (host-side probe + degrade in `remote_sync`), and
  `thegn doctor` reporting.
