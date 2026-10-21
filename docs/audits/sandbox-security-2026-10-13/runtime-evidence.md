# Local runtime evidence — 2026-10-13

Source commit: `38e09ded78652c6cf56b0e3a7b6a7c16d18dcd95`. All work ran in the isolated audit worktree. Production code and public APIs were unchanged. These probes call the checked-out `thegn-core` implementations of `resolve`, `enter_argv`, `ensure`, `Config::resolve_env`, the DNS proxy, and the isolation-floor decision.

## Safety and scope

The host was Linux/NixOS, kernel 7.2.9, with working bubblewrap, user systemd, and rootless Podman 5.8.8. The Docker command was Podman-backed, so this is not an independent Docker-engine validation. A locally cached Debian image was selected by immutable local image ID `84be7cfa538b9f8180f268e20329f0123b781d471fb490c162294e631b07f9e1`; no image was pulled or built. Synthetic `HOME`, Git repositories, a linked worktree, credential/cache canaries, and state directories lived inside temporary directories belonging to the audit worktree. A separate synthetic shared-memory canary lived under a uniquely owned `/dev/shm` directory. All fixtures were removed after each run.

Only synthetic `GH_TOKEN` was injected. Follow-up shell launches explicitly received the synthetic `HOME`; no real credential was selected for a probe. The initial systemd sealed launch inherited the user manager HOME, which was masked by `ProtectHome=tmpfs`; its probe paths remained synthetic. TCP and UDP DNS responders ran on ephemeral local ports. The only connection targets were host loopback or Podman's local host gateway. No external service received probe traffic. The audit wrapper forbids registry pulls and records OCI control-command stderr. Container names included the audit date, driver PID, and test label; cleanup removed only those exact names. No application daemon or other agent's container was restarted.

These are core-launch/runtime probes, not a UI/pane/autopilot/hook end-to-end suite. Windows, macOS/Apple, WSL, Smol, SSH, Kubernetes, provider sandboxes, VPN modes, alternate OCI runtimes, and a genuine Docker daemon were not run. Private-copy sync and recovery are proposed features and are not implemented in this checkout; the runtime probes make no claim to validate their behavior.

## Evidence sets

- [`runtime/results/`](runtime/results/) is the initial default-input run. It retains the diagnostic failures and the initial helper's unwrap panic on systemd's mount refusal.
- [`runtime/results-default-confirmed/`](runtime/results-default-confirmed/) repeats default inputs with diagnostic stderr and the `/rw/key` alias probe.
- [`runtime/results-explicit-mounts/`](runtime/results-explicit-mounts/) replaces only the malformed default GPG mount with supported same-path writable syntax, `~/.gnupg`.
- [`runtime/results-oci-minimal/`](runtime/results-oci-minimal/) additionally sets `auto_caches=false` for OCI and explicitly mounts the synthetic home read-only, synthetic GPG/cache directories writable, and `/nix/store` read-only. Sealed omits the ambient home and GPG directory. This controlled configuration permits OCI execution on this NixOS host without overmounting the image's `/bin` and `/usr`.
- [`runtime/results-final-control/`](runtime/results-final-control/) adds host shared-memory, custom-mount, policy-overlay, and process-restart controls using those supported mount configurations.
- [`runtime/results-container-restart/`](runtime/results-container-restart/) restarts only the audit-owned retained container and verifies the subsequent fresh-helper request.
- `results-corrected-mounts` records an intermediate attempt using unsupported `host:dest:rw` syntax, which silently dropped that nonexistent combined path. It must not be interpreted as the final explicit-mount run.

## Filesystem observations

| Actual launch                   | Home/sibling/SSH canary                      | Synthetic GPG key                                    | Linked worktree and shared Git                                       | Cache             | Host `/dev/shm` canary                         |
| ------------------------------- | -------------------------------------------- | ---------------------------------------------------- | -------------------------------------------------------------------- | ----------------- | ---------------------------------------------- |
| Default bwrap hardened          | Readable; writes refused                     | Home path read-only; `/rw/key` readable and writable | Readable/writable; shared `.git/config` read-only                    | Readable/writable | Hidden                                         |
| Explicit-mount bwrap hardened   | Readable; writes refused                     | Readable/writable                                    | Same                                                                 | Same              | Hidden                                         |
| Explicit-mount systemd hardened | Readable; writes refused                     | Readable/writable                                    | Same                                                                 | Same              | Readable/writable                              |
| Minimal-mount Podman hardened   | Readable; writes refused                     | Readable/writable                                    | Same                                                                 | Same              | Hidden                                         |
| bwrap/systemd/Podman sealed     | Ambient home/sibling/credential files hidden | Hidden                                               | Worktree/shared Git remain writable; shared config remains read-only | Still writable    | Systemd readable/writable; bwrap/Podman hidden |

The shared Git canary was inside the primary repository's `.git`, not a standalone sandbox repository. Mount protection of `.git/config` does not isolate objects, refs, per-worktree metadata, or hooks in the writable shared parent. These probes tested file read/write access, not an actual malicious ref/hook change.

An explicitly configured extra bind made the synthetic system fixture outside `HOME` readable and writable in bwrap, confirming that additional mounts extend the boundary. No system configuration file was modified. `/etc/os-release` existed inside the tested launches. `systemd` sealed conceals home directories but still exposes the synthetic host `/dev/shm` file: home confidentiality is not a filesystem allowlist.

The mount parser accepts `:ro` and `:cache`; `:rw` is not a recognized suffix. The configured default `~/.gnupg:rw` becomes source `HOME/.gnupg`, destination `rw`. Real bwrap accepts that relative destination as `/rw`, exposing the key there with writable access. Systemd refuses the relative destination before launching. OCI control stderr reports the invalid relative destination. The supported explicit same-path writable syntax is `~/.gnupg`, without a suffix. See [`sandbox_mounts.rs`](../../../crates/thegn-core/src/sandbox_mounts.rs) `parse_mount` around line 585 and the default configuration around `config.rs:4412`.

## Network observations

| Control                                                  | Actual outcome                                                                                                                                 |
| -------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| bwrap hardened, NAT, `network_block=["*"]`               | Controlled direct-IP TCP and alternate UDP DNS both succeed; no DNS filter is inserted.                                                        |
| bwrap/systemd fresh `network=none`                       | Controlled TCP fails; UDP DNS cannot reach the responder.                                                                                      |
| bwrap/systemd sealed, requested NAT                      | Resolved network is `None`; controlled endpoints cannot be reached.                                                                            |
| Podman minimal-mount hardened, NAT                       | Controlled local gateway TCP and alternate UDP DNS succeed.                                                                                    |
| Fresh Podman minimal-mount `network=none` or sealed      | TCP/UDP return network-unreachable; sealed resolves to `None`.                                                                                 |
| Podman hostname filter configured                        | Actual create fails: `127.0.0.1:<ephemeral-port> is not an ip address`. The runtime expects a DNS IP, not the host proxy's IP-and-port string. |
| Retained Podman NAT container; later `spec.network=None` | `ensure=ok`; same container ID and `pasta` network remain; TCP and alternate DNS still succeed.                                                |

The reuse result is repeated across a fresh helper process and an actual restart of the same audit-owned container. Mounts/home label satisfy `container_status`, so network changes are not validated before reuse. This demonstrates the stale effective policy independently of source-only command reasoning. See `sandbox.rs` `judge_inspect`, `container_status`, and `ensure` around lines 1916, 2015, and 2087.

The actual process-wide DNS server was exercised with a controlled local upstream. First-policy `allowed.test` returns RCODE 0; unlisted `blocked.test` returns NXDOMAIN/3. A second `get_or_start` requests deny-all but returns the same port; `allowed.test` still returns RCODE 0. Direct TCP to a controlled IP and direct UDP querying of the alternate resolver succeed outside any proxy enforcement. This is an actual DNS-service test, not an OCI domain-resolution success: the OCI DNS argument fails before launch. The original DNS E2E suite was not run because its allowed queries use the system/external resolver.

## Configuration, forwarding, and fallback

[`results-final-control/policy-overlay.log`](runtime/results-final-control/policy-overlay.log) shows ordinary repo network overlays narrow global policy: a disjoint allow-list or explicit empty repo allow-list adds universal block `*`; repo block entries append; an allow-list narrows an unrestricted base. These configuration checks do not establish runtime enforcement on unsupported backends.

[`env-selection.log`](runtime/results-final-control/env-selection.log) reproduces the named-environment bypass through the actual config resolver with deny-all approvals: a global `sealed`, `network=none`, `isolation_floor=shared-kernel`, `on_floor_miss=fail` config initially resolves unchanged. A checked-in `.thegn.toml` selecting preconfigured `env="hostdev"`, even while restating sealed/none restrictions, resolves to that user-defined env's `backend=none`, `profile=open`, `network=host`, `floor=off`, `miss=degrade`. The named env overlay applies after repo narrowing. No host workload was spawned by this reproduction.

All tested sealed runtimes still received the synthetic `GH_TOKEN` from configured passthrough. Hiding home files does not withhold explicitly forwarded credentials.

The constructed missing-backend request selects unavailable Apple on Linux with an empty fallback chain and requests sealed/none. Actual `resolve` emits the host-fallback warning and returns `None`; the helper intentionally does not convert that answer into a host shell. The actual pure floor function returns `Ok` for floor off, `Degrade` for shared-kernel/degrade on HostProcess, and `Fail` for shared-kernel/fail. This verifies the resolver and decision semantics, not every launcher's enforcement of the decision.

## Launch limitations and existing tests

Default OCI attempts on this NixOS host also fail because host-toolchain `/bin` and `/usr` overmount the image toolchain, leaving its `sleep` entrypoint unavailable. Captured errors additionally include a `/lib64` mount-creation failure on some attempts. Those failures are preserved; successful OCI observations use the explicitly documented minimal mount configuration. No unavailable backend is credited with a runtime result.

Offline compilation succeeded in a private target directory, which was removed after validation; reproducible sources and captured output are retained. Existing integration results:

- `sandbox_profile`: 3 reported passes; its Podman lifecycle test returns early under `SKIP_PODMAN_E2E=1`.
- `sandbox_readonly_home`: 4 passes.
- `sandbox_sealed_home`: all 27 tests pass when each test runs in its own process, including the real synthetic-home bwrap test.

The original shared-process run and supported-home retry each report 26 passes/1 failure in `planned_view_separates_confidentiality_from_integrity`. Several tests mutate process `HOME` without restoring it and explicitly assume nextest's per-test process isolation. Merely moving the initial HOME under `/home` does not fix the shared-process runner. The recorded per-test run honors that assumption; it is not a claim that the original full-suite invocation passed. Logs for both failures and the passing isolated run are retained.

## Reproduction

Use an isolated worktree, an already cached image, and a local rootless Podman socket. Override the image and socket arguments for another machine. The scripts remove only their unique container names and synthetic fixtures.

```sh
CARGO_TARGET_DIR="$PWD/.audit-target" cargo build --offline \
  --manifest-path docs/audits/sandbox-security-2026-10-13/runtime/Cargo.toml
python3 docs/audits/sandbox-security-2026-10-13/runtime/run-probes.py \
  --helper .audit-target/debug/thegn-sandbox-audit-probe \
  --corrected-mounts --minimal-oci \
  --output docs/audits/sandbox-security-2026-10-13/runtime/reproduced
```

Omit `--corrected-mounts --minimal-oci` to reproduce configured defaults. `--only` accepts comma-separated case labels. The helper uses real core APIs; the Python driver supplies synthetic inputs, PTYs, controlled endpoints, and bounded execution. Read the individual logs' `request`, `mounts`, `argv`, refusal/error, and probe lines together before interpreting a result.

## Regression canaries (2026-10-21, THE-792)

These four canaries are positive controls on the current code: each passes on main and fails if its named fix is reverted. They run through core APIs without starting a host workload or an OCI runtime.

| Reproduction                                                                                                                                         | Canary and pre-existing regression tests                                                                                                                                                                                                                    |
| ---------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Repo `env = "hostdev"` cannot weaken a sealed global sandbox; explicit user selection remains available.                                             | `sandbox::tests::evidence::canary_env_selection_repo_hint_cannot_select_a_weaker_named_env`; `config_resolve::tests::repo_env_hint_*` — guards the repo env-hint weakening clamp in `config_resolve.rs`.                                                    |
| Missing terminal backend cannot silently fall through to a host process under the default floor; explicit degrade and off remain deliberate choices. | `sandbox::tests::evidence::canary_terminal_fallback_default_floor_refuses_host_process`; `panes.rs::terminal_unknown_backend_*` — guards default-floor `Fail` policy. The canary checks the pure floor decision; the pane tests exercise launcher behavior. |
| A retained NAT container cannot be reused for a `network = none` request.                                                                            | `sandbox::tests::evidence::canary_reuse_narrowing_network_change_forces_recreate`; `sandbox_tests::judge_inspect_rejects_policy_mismatch` — guards policy-fingerprinted reuse in `judge_inspect`.                                                           |
| A second deny-all DNS policy gets its own filter and blocks a name allowed by the first policy.                                                      | `sandbox::tests::evidence::canary_dns_singleton_second_policy_gets_its_own_filter`; `dns_filter::tests::find_or_plan_reuses_equal_key_starts_new_and_caps` — guards per-policy DNS instance keys.                                                           |

Run the canaries with:

```sh
cargo nextest run -p thegn-core -E 'test(sandbox::tests::evidence::)'
```
