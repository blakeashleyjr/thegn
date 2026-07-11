# Tasks

## 1. svc: the vps module (Hetzner end-to-end)

- [x] 1.1 `vps/hetzner.rs` — pure request shaping (URLs, create/list/destroy,
      ssh keys, snapshot/create_image, label selector) — **unit tests**: URL
      shapes, body fields, snapshot-id-as-number, parse envelopes, pubkey match.
- [x] 1.2 `vps/registry.rs` — file ledger under `$XDG_STATE/thegn/vps/`
      (intent → ready → removed; per-instance known_hosts) — **unit tests**:
      round trip, corrupt/foreign files skipped, idempotent remove.
- [x] 1.3 `vps/ssh_shim.rs` — exec (secrets over stdin, ControlMaster,
      kill_on_drop) + ProviderFiles (read/write/list/delete) — **unit tests**:
      argv normalization, exports preamble, base argv, find-listing parse.
- [x] 1.4 `vps/cloudinit.rs` — keys-only vs stock-image (nix prereqs + docker)
      user-data — **unit tests**: both shapes.
- [x] 1.5 `vps/mod.rs` — `VpsProvider`: create (cap check → intent → key →
      POST → poll running+IP → TCP :22 → cloud-init settle → finalize),
      destroy (ledger/API id resolve, transient-status retries, ledger clear),
      list (label-filtered), `resolve_ip`, `poweroff`/`snapshot` for bake.
- [x] 1.6 `Provider::Vps` variant + dispatch arms + `caps()` (files only) in
      `provider.rs`; `tests/vps_mock.rs` replay test (create/list/destroy +
      ledger flow) mirroring `sprites_mock.rs`.

## 2. core: config + placement

- [x] 2.1 `EnvProviderConfig`: `region`, `size`, `max_instances`,
      `max_lifetime_secs`; `vps_provider_kind()`;
      `control_command_template()` (the `thegn vps-ssh {id} --` default).
- [x] 2.2 `envbuild`: VPS envs with no `exec_command` get the self-bridge as
      control/interactive prefix — **unit test**: prefix shape, explicit
      exec_command wins, non-VPS unchanged.
- [x] 2.3 `envplan::bake_scripts()` — the repo-independent provision prefix —
      **unit test**: nix + direnv only, installer/parallel honored, no clone.

## 3. host: wire-up + leak safety + bake

- [x] 3.1 `provider_factory.rs` (extracted from the pinned `agent.rs`; ratchet
      re-baselined lower) with the hetzner arm over the managed keypair;
      `cmd/env.rs::api_provider` VPS arm.
- [x] 3.2 `vps_bridge.rs` + hidden `thegn vps-ssh` subcommand (ledger → API
      fallback IP resolve, `-tt` under a PTY, process exec).
- [x] 3.3 Checkpoint plan step gated on `caps().checkpoints`; warm-pool claim
      rebind uses `control_command_template` — **unit test**
      (`lifecycle.rs::vps_spares_destroy_instead_of_recycling`): VPS spares
      destroy, never recycle.
- [x] 3.4 `vps_reaper.rs` — label+host-scoped orphan/lifetime reaper on the
      hydration cadence (self-throttled 5 min, network on its own thread),
      called from `hydrate::build_model`.
- [x] 3.5 `cmd/env_image.rs` + `thegn env image-bake` (bake → poweroff →
      snapshot → destroy → print `template = "snapshot:<id>"`).

## 4. Docs + validate

- [x] 4.1 `config/config.toml.example`: the `[env.hetzner]` example with the
      cost model, speed tiers, and the not-available list.
- [x] 4.2 `just lint` + `cargo test --workspace` + `just coverage` (core ≥95%) + `just smoke` green; ratchet re-baselined for the agent.rs shrink.
- [ ] 4.3 Live verification against a real Hetzner account (needs
      `HCLOUD_TOKEN`): provision → interactive pane → `env down` → console
      empty; leak drill (kill between intent and finalize → reaper destroys);
      pool claim ~5 s; bake → cold open ≤ ~40 s.
