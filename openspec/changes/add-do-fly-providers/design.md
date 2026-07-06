# Design

## Provider seam recap

`RemoteProvider` + capability sub-traits (`ProviderFiles`, checkpoints, …) is
the extension point; the provisioning pipeline is gated only on `caps().files`.
Both new backends satisfy `files` over the managed-ssh exec/files shim, so no
pipeline code changes. Neither touches the event loop's render damage channels
(all network work is off-loop, on spawned threads or `spawn_blocking`), and
neither adds a SQLite schema change — the instance ledger stays file-based
under `$XDG_STATE/superzej/vps/`.

## DigitalOcean: a second VpsKind

`vps/mod.rs` factors the vendor-specific request shaping behind a `VpsShaper`
trait (create/list/destroy bodies + URLs, ssh-key registration, snapshot,
label/tag selector, parse envelopes). Hetzner's existing shaping becomes one
impl; `vps/digitalocean.rs` is a second (droplets, `tag:superzej-managed`
selector, snapshot-by-action-poll). `VpsKind::parse` recognizes `digitalocean`;
everything else (ledger, reaper, self-bridge, bake) is shared and unchanged.

**Live gotchas already resolved:** Hetzner `cx22` is deprecated → default
`cx23`; ssh-key registration is 409-idempotent by naming the key
`superzej-managed-<fingerprint>` (re-use, don't recreate).

## Fly: a distinct RemoteProvider

Fly's unit is an **app + one Machine**. `fly/machines.rs` drives
`api.machines.dev` (create app, allocate/attach the machine, stop/start,
destroy); `fly/graphql.rs` calls `api.fly.io` GraphQL solely to allocate a
**dedicated IPv4** (the Machines REST API cannot). Reachability is the
allocated public IPv4 + a guest `sshd` + the managed keypair — the identical
ssh transport the VPS backends use — so `caps().files` holds and the pipeline
runs unchanged.

Scale-to-zero: a stopped Fly Machine is near-free, so
`provider_scale_to_zero("fly")` is true and idle ⇒ **stop** (not destroy);
`max_lifetime_secs` is still the hard destroy ceiling (enforced by
`fly_reaper`).

**Live gotchas already resolved:** stop/start returns 412 unless the machine
has restart policy `no` and you poll real state after SIGTERM; `wait_reachable`
must gate on ssh `exit == 0` (255 = not-yet-up, not success); Docker's overlay
driver fails on Fly's rootfs, so the sshd init presets `storage-driver=vfs`.

## Baked image fast path

`nix/fly-sandbox-image.nix` (`streamLayeredImage`) bakes the rust toolchain and
an sshd entrypoint (privsep `sshd` user + `/var/empty`) into an image published
to Fly's registry (`just fly-image-publish`). Setting
`template = "image:<ref>"` makes a machine boot ready, replacing a per-cold-start
toolchain build.

## Leak safety

Fly is not a `VpsKind`, so `vps_reaper` does not cover it. `fly_reaper` mirrors
it against ledger records tagged `provider = "fly"`: destroy past
`max_lifetime_secs` (a running machine bills), reap stale-`creating` records
(crash between intent-write and finalize), leave a healthy record under the
ceiling alone. Self-throttled to 300s; network work on its own thread; called
from the hydration cadence. DO reuses `vps_reaper` unchanged.
