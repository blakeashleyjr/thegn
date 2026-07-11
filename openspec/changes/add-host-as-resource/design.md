# Design

Full implementation plan (types, signatures, flows) lives with the change; this
records the load-bearing decisions.

## State machine (pure, thegn-core)

```
Unknown → Connecting → Probing ─ runtime present ─────────────→ RuntimeReady*
                          └─ absent → AwaitingConsent → Installing → RuntimeReady*
RuntimeReady → ImageResolving ─ digest in inventory ─→ ImageReady*
                          └─ missing → Delivering{strategy} ─→ ImageReady*
ImageReady → VolumeSeeding → Ready*          (any) → Failed{step, error, retryable}*
```

`*` = durable checkpoint; only durable states persist, so a killed driver
resumes from the nearest checkpoint by construction. `host_machine::step(state,
ctx, event) -> Transition{next, effects}` is pure and total (illegal pairs →
`Failed`, never a wedge); effects are data (`Connect`, `Probe`, `Deliver{plan}`,
`Checkpoint`, `Emit`), executed by an impure driver. `Ready` + fresh probe TTL
(default 900s) is a no-op; stale → cheap re-probe; `Failed{retryable:false}`
sticks until explicit user action.

## Single-flight

The driver lives in `thegn-host/src/host_flow.rs` (blocking, spawn_blocking
context — matching all existing provisioning), not an async supervisor:
`provision_gate`-style `host_lock(host_id)` plus a Flight registry
(leader/follower on a Condvar; followers forward each progress snapshot to
their own tab's splash callback). Cross-process (TUI vs CLI): the DB row is the
arbiter — the leader heartbeats each step; a fresh heartbeat means "attach and
render the persisted steps", a stale one means "take over from the last durable
checkpoint". `ensure_ready(host)` always completes before any
`sandbox_lock(name)` is taken (coarser gate first, no nesting).

## Delivery (default: registry-less over SSH, with true byte resume)

Raw `podman save | ssh | podman load` cannot resume. Instead: local
content-addressed oci-archive cache → remote offset query (`stat` the
`.partial`) → append the remainder over the multiplexed master (`cat >>`, or
rsync `--partial --inplace` when both ends have it) → sha256 verify →
`podman load` + digest `image exists` verify. Strategy ladder (pure
selection from probed caps + config prefs): SshStream+rsync > SshStream >
RegistryPull > SkopeoRemoteCopy > RemoteBuild; cloud lowers to
ProviderTemplate. Registry failure with transfer available is a fallback, not
a failure.

## Multi-arch image model

Base image is an amd64+arm64 **manifest list**; `ResolvedImage` maps the list
digest to per-arch digests. Inventory and spec pinning key on the **per-arch**
digest (what actually exists on a host); the list digest is provenance. Base
image is nix-built (`nix/sandbox-image.nix`, dockerTools) with a Containerfile
fallback for the RemoteBuild strategy.

## Consent

The machine cannot enter `Installing` without a `ConsentGrant`, producible only
by config `install_runtime = "auto"`, an interactive confirm modal, or CLI
`--yes`/tty prompt. Grants persist on the `hosts` row (per-machine state, not a
config write). Background paths (eager, warm pool) never prompt and never
install — they defer to focused materialize.

## Warm volumes

`thegn-nix-store → /nix` and `thegn-cargo → ~/.cargo` as named volumes,
seeded by **image copy-up** by default (the base image's /nix IS the seed; zero
extra transfer) with a tarball `volume import` variant for big stores.
Per-worktree `target/` stays inside the worktree mount.

## Reach lowering

`Reach::Iroh` lowers to SSH over a `dumbpipe connect-tcp` local forward before
anything else sees it (probe/install/delivery/spawn are byte-identical to SSH);
the tunnel child's lifetime is tied to a host lease held while sandboxes live.
Cloud reach synthesizes caps (`CloudManaged` runtime, ProviderTemplate
delivery): Sprites `ensure_image` = base-plan checkpoint; Daytona = snapshot
registration referencing the published list digest.
