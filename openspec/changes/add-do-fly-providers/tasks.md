# Tasks

## 1. svc: DigitalOcean as a second VpsKind

- [x] 1.1 Extract a `VpsShaper` trait in `vps/mod.rs` (create/list/destroy
      bodies + URLs, ssh-key register, snapshot, tag selector, parse envelopes);
      make Hetzner one impl — **unit tests** stay green (URL/body shapes).
- [x] 1.2 `vps/digitalocean.rs` — the DO `VpsShaper` impl (droplets,
      `tag:thegn-managed` selector, snapshot-by-action-poll, key register) —
      **unit tests**: URL shapes, body fields, tag selector, parse envelopes.
- [x] 1.3 `VpsKind::parse` recognizes `digitalocean`; `token_env_default` →
      `DIGITALOCEAN_TOKEN`; `tests/vps_do_mock.rs` replay test.

## 2. svc: Fly as a distinct RemoteProvider

- [x] 2.1 `fly/machines.rs` — Machines REST API (create app + machine,
      stop/start with restart-policy `no` + real-state poll, destroy, list) —
      **unit tests**: request shapes, state parsing.
- [x] 2.2 `fly/graphql.rs` — dedicated-IPv4 allocation via `api.fly.io` GraphQL
      — **unit tests**: query shape, response parse.
- [x] 2.3 `fly/mod.rs` — `FlyProvider` (`Provider::Fly`; caps `files`;
      scale-to-zero): create → alloc IPv4 → guest sshd reachable (gate on ssh
      `exit == 0`) → pipeline; ssh exec/files over the managed keypair.
- [x] 2.4 `Provider::Fly` variant + dispatch arms in `provider.rs`;
      `tests/fly_mock.rs` replay test (create/stop/start/destroy + ledger).

## 3. core + host: config, factory, reaper, bake

- [x] 3.1 core: provider kinds recognize `digitalocean`/`fly`;
      `provider_scale_to_zero("fly")` true — **unit test**.
- [x] 3.2 `provider_factory.rs`: `digitalocean` VPS arm + `fly_provider_for`
      (shared by launch path and reaper) — **unit test** (missing token →
      None).
- [x] 3.3 `fly_reaper.rs` — the Fly ledger reconciler (destroy past
      `max_lifetime_secs`, reap stale-`creating`), self-throttled 300s, network
      on its own thread, called from `hydrate::build_model`.
- [x] 3.4 `nix/fly-sandbox-image.nix` (`streamLayeredImage`: rust toolchain +
      sshd entrypoint) + `flake.nix` `packages.fly-sandbox-image` + `just
fly-image-publish`; docker init presets `storage-driver=vfs`.

## 4. Docs + validate + live

- [x] 4.1 `config/config.toml.example`: `[env.digitalocean]` and `[env.fly]`
      examples (token source, region, size, `template = "image:<ref>"`, the
      scale-to-zero vs destroy note).
- [x] 4.2 `cargo test --workspace` + clippy `-D warnings` + ratchet green.
- [x] 4.3 Live verification: DigitalOcean + Hetzner (create → ssh → destroy) and
      Fly control plane (create → stop/start → destroy), accounts confirmed
      empty afterward (no leaked droplets/apps/IPs/keys).
- [ ] 4.4 `just ci` green (fmt + lint + build + test + openspec-validate +
      coverage + smoke + nix-build).
