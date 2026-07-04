# Add commodity-VPS execution backends (Hetzner first)

## Summary

Add cheap commodity-VPS vendors as managed sandbox providers, starting with
**Hetzner Cloud** (CX22 ≈ €4/mo, ~10 s create→running): superzej provisions an
instance via the vendor's REST API, reaches it over plain ssh (a `szhost
vps-ssh` self-bridge — no vendor CLI), and runs the standard provisioning
pipeline on it. Native REST (reqwest, mirroring `SpritesProvider`/
`DaytonaProvider`) was chosen over Pulumi/OpenTofu: the operation is one
authenticated POST + a status poll, superzej already owns sandbox state, and
Pulumi has no Rust SDK (CLI + language host + plugins + a second state store).

The cost model differs structurally from Sprites: **a VPS has no
suspend/checkpoint — a powered-off instance still bills — so the only free
state is destroyed.** The design therefore never offers stop-on-idle, ledgers
every create _before_ the API call, runs a label-scoped orphan reaper, and
replaces the checkpoint speed-path with a baked base image
(`superzej env image-bake`) plus the existing warm pool (whose recycle path
falls through to destroy for checkpoint-less spares).

## Impact

- tasks.md: **AE 749** (Container provisioning — commodity-VPS provider
  backend); builds on AE 386 (warm-spare pool) and the `[env.<name>]` named
  execution environments; complements
  `add-remote-provision-hooks` (BYO-infra hooks) with a first-class budget
  backend.
- **superzej-svc** — new `vps` module: `VpsProvider` (`Provider::Vps` variant;
  caps `files` only), pure Hetzner shaping, ssh exec/files shim, cloud-init
  builder, file-based instance ledger under `$XDG_STATE/superzej/vps/`.
- **superzej-core** — `EnvProviderConfig` gains `region`, `size`,
  `max_instances`, `max_lifetime_secs`; `vps_provider_kind()`;
  `control_command_template()` (the `szhost vps-ssh {id} --` default prefix);
  `envplan::bake_scripts()`.
- **superzej-host** — `provider_factory.rs` (extracted from the pinned
  `agent.rs`), `vps_bridge.rs` (`szhost vps-ssh`), `vps_reaper.rs`
  (hydration-cadence, self-throttled), `cmd/env_image.rs`
  (`superzej env image-bake`).
- **No DB schema change** — the instance ledger is file-based (svc and the CLI
  bridge both read it; every create/destroy flows through the provider).

## Rationale

Managed sandbox providers are convenient but expensive relative to commodity
VPS. The provider seam (`RemoteProvider` + capability sub-traits + the
transport-agnostic provisioning pipeline gated on `caps().files`) was built for
exactly this: a new backend is one enum variant + its sub-trait impls. The ssh
shim satisfies the `files` capability, so the entire pipeline (clone → nix →
dotfiles → agents → parity) runs unchanged; the `vps-ssh` self-bridge gives the
placement a CLI prefix, so panes, chrome git/fs reads, the persisted worktree
location, and the warm-pool claim rebind all work through existing code paths.

## Non-goals

- **DigitalOcean / Vultr adapters** — the shared core (`VpsKind`, shim, ledger,
  reaper) is built for them, but they ship as a follow-up (~150 lines of pure
  shaping each).
- **Cloud firewall + spend UI** — Phase 3 (a follow-up change): vendor firewall
  rules, a $/hr estimate in the sandbox detail pane.
- **Checkpoint emulation via vendor snapshots** — deliberately rejected:
  minutes-slow, storage churn, IP changes; the baked image + pool cover speed.
- **Suspend-on-idle** — structurally impossible to do for free on a VPS; the
  design must never present a stopped-but-billing instance as "suspended".
