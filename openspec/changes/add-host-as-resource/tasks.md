# Tasks

## 1. Spine (thegn-core + wiring headroom)

- [x] 1.1 Ratchet payments: extract run.rs provision/spec drains →
      `handlers/provision.rs`; config.rs env sub-tables →
      `config_env_tables.rs` (pub use re-exports); db.rs migration helpers →
      `db_migrate.rs`; `test/file-size-ratchet.sh --update`.
- [x] 1.2 Core `host.rs` (HostId/Reach/Arch/RuntimeInfo/DeliveryCap/HostCaps
      probe parser/VolumeSpec/apply_ready_host) — **unit tests**: probe-output
      fixtures, HostId canonical round-trips, anonymous-id stability.
- [x] 1.3 Core `host_machine.rs` (pure step/resume/next_strategy) — **unit
      tests**: exhaustive state×event table, resume matrix, consent ladder,
      strategy fallback, retryable taxonomy, illegal-event totality.
- [x] 1.4 Core `image.rs` (Digest/ImageRef/ResolvedImage manifest-index parser,
      per-arch pinning, select_delivery) + `inventory.rs` (missing/
      needs_reverify) — **unit tests**: OCI index + Docker list + single-manifest
      fixtures, selection matrix.
- [x] 1.5 Core `host_config.rs` (`[host.<name>]`, `[env.<name>] host = "..."`,
      resolve_host_binding rules incl. implicit anonymous hosts, repo-overlay
      exclusion) — **unit tests**: rules 1–5, undefined-host warn, secrets refs.
- [x] 1.6 DB v29 (`hosts`/`host_inventory`/`host_events`) in new `host_db.rs`;
      db.rs gets version bump + `conn()` accessor only — **unit tests**:
      round-trips, migration idempotence, stale sweep.
- [x] 1.7 Host spine: `host_flow.rs` (ensure_ready + Flight + consent tickets +
      splash-step mapping + provision_worktree wrapper + host_pending),
      `provision_gate::host_lock`, `handlers/host.rs` (HostRuntime drain),
      run.rs wiring (net-negative), `host_ui.rs`, `Section::Hosts` panel
      section + DetailActions, sidebar HOSTS rows, `thegn host` CLI.

## 2. SSH remote podman

- [x] 2.1 svc `host/` module: HostChannel (Local/Ssh/Mock, exec/exec_streamed/
      oci_url), CliConnector (open/probe over ssh_base ControlMaster),
      probe script, RegistryPull delivery, ImageCopyUp volume seeding —
      **mocked-channel tests**: probe, single-flight, golden second-call no-op.
- [ ] 2.2 Wizard host readiness badges + tabbar chip decoration; SandboxHalt
      surfacing for host failures.

## 3. Consented runtime bootstrap

- [x] 3.1 `ensure_runtime` distro-detected install (apt/dnf/apk/pacman,
      linger, subuid/subgid, podman socket); AwaitingConsent → confirm modal /
      DB grant / CLI `--yes`; BackgroundSkip for eager+pool — **tests**:
      consent ladder end-to-end with mock channel, never-silent invariant.

## 4. Registry-less transfer (default delivery)

- [x] 4.1 SshStream: local oci-archive cache, offset query, streamed append
      (+ rsync variant), sha256 verify, load + digest verify + managed tag;
      Skopeo + RemoteBuild strategies — **chaos test**: kill mid-transfer →
      resume from offset (`resumed@` event), hash-mismatch → clean retry.
- [x] 4.2 Byte progress + stall rule (120s), `rm-cache` action, digest-mismatch
      fatal path; smoke golden-path: second provision reports zero transfers.

## 5. Warm volumes + base image

- [x] 5.1 `nix/sandbox-image.nix` multi-arch base (nix/devenv/rust, uid-1000,
      chowned /nix + ~/.cargo) + Containerfile fallback + justfile
      `image-build`/`image-publish` + CI publish (manifest list, digest bump).
- [ ] 5.2 Volume seeding: copy-up default + tarball import variant; cargo
      volume; pool gate (`reconcile_pool` waits for host Ready).

## 6. iroh reach

- [x] 6.1 `Reach::Iroh` → dumbpipe connect-tcp lowering (port scrape,
      HostKeyAlias, lease-holds-tunnel, reconnect backoff) — **mocked tests**.

## 7. Cloud lowering

- [ ] 7.1 `host/cloud.rs`: synthesized caps, Sprites checkpoint lowering
      (base-plan hash keys), Daytona snapshot registration — **REST-stub tests**.

## 8. Docs + validate

- [x] 8.1 Document `[host.<name>]` + `[env.<name>] host` in
      `config/config.toml.example`; `thegn doctor` host summary optional.
- [ ] 8.2 `just ci` green per phase (fmt, lint incl. ratchet, build, test,
      coverage ≥95% core, openspec validate, smoke incl. host golden path).

## 9. Batteries + sprites (hosts v2)

- [x] 9.1 In-TUI/CLI host add (DB defs merged into the config catalog, wizard
      "+ add host…" row, `thegn host add/rm`, doctor Hosts section).
- [x] 9.2 Detection extensions (deno/scala/shell.nix) + `Tier::SynthNix` +
      pure `toolchain.rs` synthesis + `[toolchain]` config.
- [x] 9.3 Host-backed per-worktree pipeline (`host_provision.rs`): remote
      detect probe, toolchain + personal layer over exec, tar-over-exec file
      delivery, synthesized-devshell pane entry via spec init_script.
- [x] 9.4 Sprites S1 (checkpoint ids captured + persisted) + S2 (stale-spare
      and delete-path recycle via restore-in-place, lock-hash guarded) + S3
      (explicit `[host.*] reach="cloud"` engages the cloud runner; implicit
      provider envs stay on the legacy pipeline pending live verification).

## 10. Sprites live verification (2026-07-03)

- [x] 10.1 Live-verified with SPRITES_TOKEN: baseline lifecycle + provision
      green; recycle restore-in-place ~233s→12s (~19x); claimed-delete round
      trip; bad-checkpoint destroy fallback. Fixes found live: checkpoint
      existence guard before restore + `[lifecycle.pool] recycle` kill-switch.
      Follow-up: marker written post-checkpoint (efficiency-only; claim skips
      provisioning).
