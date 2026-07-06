# Add DigitalOcean and Fly.io execution backends

## Summary

Extend the managed-sandbox provider seam with two more budget backends:

- **DigitalOcean** — a second `VpsKind` alongside Hetzner. The VPS core
  (`VpsProvider`, ssh exec/files shim, intent-ledger, label-scoped reaper,
  `szhost vps-ssh` self-bridge, `env image-bake`) is vendor-agnostic; DO is
  ~one `VpsShaper` impl (pure request shaping: droplets, ssh keys, snapshots,
  tag selector), exactly the follow-up the VPS change named as a non-goal.
- **Fly.io** — a CLI-free first-class `Provider::Fly`, not a `VpsKind`. Fly's
  primitive is a Machine (an app-per-sandbox), created over the Machines REST
  API (`api.machines.dev`) with a dedicated IPv4 allocated via the Fly GraphQL
  API. Reachability is **public IPv4 + a guest sshd + the managed keypair**
  (the same ssh transport the VPS backends use), _not_ WireGuard: the host has
  no reliable IPv6 egress, and a proven public-IP+sshd path runs the standard
  provisioning pipeline unchanged. A stopped Fly Machine is near-free, so Fly
  is scale-to-zero (stop/start), unlike a VPS.

Both reuse the transport-agnostic provisioning pipeline gated on
`caps().files`, so clone → nix → dotfiles → agents → parity run unchanged, and
panes/chrome-reads/persisted-location route through the ssh self-bridge.

To keep per-provision cost low, Fly gains a **baked-image fast path**: a Nix
image (`nix/fly-sandbox-image.nix`, the rust toolchain + sshd baked in) pushed
to Fly's registry and referenced as `template = "image:<ref>"`, so a machine
boots ready instead of building a toolchain on every cold start.

## Impact

- tasks.md: **AE 756** (DigitalOcean + Fly.io provider backends); builds on
  **AE 749** (the Hetzner/VPS core this generalizes) and **AE 386** (warm-spare
  pool). Complements `add-vps-providers` (whose non-goals named DO + a second
  vendor as the follow-up) and `add-env-setup-ux` (the authoring UX that
  surfaces all providers uniformly).
- **superzej-svc** — `vps/digitalocean.rs` (a `VpsShaper` impl) + the
  `VpsShaper` trait extraction in `vps/mod.rs`; a new `fly` module
  (`fly/machines.rs` Machines API, `fly/graphql.rs` IPv4 alloc, `fly/mod.rs`
  `FlyProvider`) with a `Provider::Fly` variant (caps: `files`; scale-to-zero).
- **superzej-core** — `EnvProviderConfig` provider kinds recognize
  `digitalocean` and `fly`; `provider_scale_to_zero()` returns true for `fly`
  (stop, don't destroy, on idle).
- **superzej-host** — `provider_factory.rs` gains a `fly_provider_for` builder
  and the `digitalocean` VPS arm; `fly_reaper.rs` (the Fly counterpart to
  `vps_reaper`, since Fly is not a `VpsKind`).
- **Nix** — `packages.fly-sandbox-image` (`nix/fly-sandbox-image.nix`) +
  `just fly-image-publish`.
- **No DB schema change** — DO reuses the file-based VPS ledger; Fly writes to
  the same `vps::registry` ledger tagged `provider = "fly"`.

## Rationale

The VPS backend was built so a new commodity vendor is one `VpsShaper` impl;
DigitalOcean exercises that seam and proves it. Fly is structurally different
(app-per-machine, scale-to-zero, GraphQL for IPv4), so it is its own
`RemoteProvider` rather than a `VpsKind` — but it satisfies the same `files`
capability over the same managed-ssh transport, so the pipeline, panes, chrome
reads, and warm-pool rebind all run through existing code paths.

## Non-goals

- **WireGuard/Fly private-network reachability** — deliberately rejected: the
  host has no reliable IPv6 egress and the public-IPv4+sshd path is verifiable
  today. Revisit only if a WireGuard data plane becomes necessary.
- **Vultr / Linode adapters** — the `VpsShaper` seam now has two impls proving
  the shape; more vendors are trivial follow-ups.
- **Fly Volumes / persistent disks** — sandboxes are ephemeral; the baked image
  plus the warm pool cover cold-start speed without persistent state.
