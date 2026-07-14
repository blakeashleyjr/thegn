# Tasks

## 1. svc: MCP-over-HTTP client

- [x] 1.1 `machine0/mcp.rs` — `Mcp0Client` (reqwest; `x-api-key`; best-effort
      `initialize` + `Mcp-Session-Id`; `call_tool` handling `application/json`
      **and** one-shot `text/event-stream`; retry budget reusing
      `provider::{transient_status,transient_err,CONTROL_TIMEOUT}`). Reuse the
      JSON-RPC types in `thegn_core::mcp::protocol`.
- [x] 1.2 Pure helpers `tools_call_body` / `parse_rpc_body` / `sse_last_data` /
      `unwrap_tool_result` (structuredContent / content-text / isError) —
      **unit tests** (no live endpoint).

## 2. svc: Machine0Provider (MCP control plane + ssh data plane)

- [x] 2.1 `machine0/mod.rs` — `Machine0Provider` + `Machine0Spec`; MCP lifecycle
      (`vm_create`/`vm_get_by_name`/`vm_list`/`vm_destroy`), spend guardrail,
      wait-running + wait-reachable (TCP :22), `SandboxHandle` with
      `ExecKind::Ssh`.
- [x] 2.2 ssh data plane via `vps::ssh_shim::SshShim` — `run_exec`, `ProviderFiles`
      (`read`/`write`/`write_exec`/`list_dir`/`delete` + default `upload_dir`/
      `download_dir`); import thegn's managed key via `ssh_key_create`/`ssh_key_list`.
- [x] 2.3 `ProviderCheckpoints` — `checkpoint` → `image_create`,
      `list_checkpoints` → `image_list`, `restore` → destroy + recreate from image.
- [x] 2.4 Scale-to-zero — concrete `suspend`/`resume` (`vm_suspend`/`vm_start`);
      resume-if-suspended before ssh.
- [x] 2.5 NixOS `provision_nixos` — `nixos-rebuild switch --flake <ref>` over ssh
      (local `path#attr` uploaded first; URL verbatim), bounded timeout.
- [x] 2.6 Pure helpers `vm_create_args` / `parse_vm` / `parse_vm_list` /
      `parse_image_list` / `parse_ssh_key_list` / `parse_created_id` /
      `status_running` / `status_suspended` — **unit tests**.

## 3. svc: Provider enum wiring

- [x] 3.1 `Provider::Machine0` variant + `caps()` (files, checkpoints,
      scale-to-zero) + all dispatch arms (create/destroy/list/name/read/write/
      write_exec/upload_dir/download_dir/checkpoint/list_checkpoints/restore/
      run_exec); `pub mod machine0` in `lib.rs`.

## 4. core: config + classification

- [x] 4.1 `EnvProviderConfig.provision_flake` + `is_default()`; docs.
- [x] 4.2 `provider_scale_to_zero("machine0") == true`; `vps_provider_kind` /
      `wss_native_provider_kind` unchanged — **unit test** locks the
      classification and the `provision_flake` is_default branch.

## 5. host: factory + CLI

- [x] 5.1 `provider_factory.rs` — `machine0_provider_for` (key `MACHINE0_API_KEY`;
      managed keypair via `sprite_ssh_keypair`) + the `machine0` arm; factory test
      (missing key ⇒ None).
- [x] 5.2 `cmd/env.rs` — `create_env` `"provider"` placement arm + `api_provider`
      arm + supported-list message.

## 6. docs + config example

- [x] 6.1 `config/config.toml.example` — documented `[env.machine0.provider]`
      block (MCP endpoint, NixOS image + `provision_flake`, capabilities).

## 7. dynamic size selection + defaults (added)

- [x] 7.1 Multidimensional size resolution: `SizeReq`
      (min_vcpu/min_ram_gb/min_disk_gb/gpu/nvme) + `size_list` → `pick_size`
      (cheapest meeting all dims); `size = "auto"`/empty triggers it, else the
      explicit size wins. Config fields on `EnvProviderConfig`; **unit-tested**
      against real `size_list` data.
- [x] 7.2 Default image `nixos-25-11-loaded` (NixOS 25.11 + modern shell/dev
      tools) when `template` unset; default region `us-east` (machine0 requires
      one). Per-VM `defaultSSHUsername` used for the ssh user.

## 8. live-hardening (from a real machine0 run)

- [x] 8.1 Correct tool names/schemas verified against a live `tools/list`:
      `ssh_key_create_public` (+`fileName`), `image_create {instanceName,imageName}`
      from a STOPPED vm, `size_list`.
- [x] 8.2 Business errors (`{error,message}` in a 200 result) surfaced as failures;
      transitional/startable status handling; poll loops tolerate transient blips.
- [x] 8.3 MCP client: dedicated no-keep-alive HTTP client + broadened retry
      (fixes mid-poll "error sending request" from reset pooled sockets).
- [x] 8.4 Shared ssh-shim hardening (also benefits VPS/Fly): hermetic
      `-F /dev/null` (ignore the user's ~/.ssh/config — a nix-store/home-manager
      config is root-owned and rejected by OpenSSH); short `$XDG_RUNTIME_DIR`
      ControlPath (deep home dirs blew the Unix-socket length cap); file writes
      base64-embedded on stdin (ssh space-joins argv, mangling `sh -c <script>`).

## 9. interactive gaps: pane bridge, auto-suspend, mosh (added)

Probing the interactive story exposed that the base provider only wired the
control/exec API — the pane, idle-suspend, and transport were incomplete.

- [x] 9.1 **`machine0-ssh` pane bridge** — `control_command_template("machine0")`
      → `[thegn, machine0-ssh, {id}, --]` so the interactive pane AND chrome
      git/fs reads reach the VM (they previously ran on the HOST via an empty
      prefix). New `crates/thegn-host/src/machine0_bridge.rs` (mirrors `vps_bridge`):
      resolves IP+user via the provider (`resolve_endpoint` wakes for a pane;
      `peek_endpoint` never wakes for a control read), small on-disk `(ip,user)`
      cache, hidden `machine0-ssh` clap subcommand.
- [x] 9.2 **Real auto-suspend/resume** — `Provider::suspend/resume` enum dispatch
      (machine0 `vm_suspend`/`vm_start`, fly stop/start); `provider_self_suspends`
      classifier (sprites only); `lifecycle::reconcile` explicitly parks idle
      non-self-suspending scale-to-zero VMs via `block_on_provider`. Resume is
      free: the pane bridge's `resolve_endpoint` wakes a parked VM on open.
- [x] 9.3 **mosh default for provider panes** — `[env.<name>.provider] transport`
      (default `mosh`); the bridge prefers mosh (`mosh --ssh=…`) for an
      interactive pane when the local client + the VM's `mosh-server` are present,
      auto-falling-back to ssh otherwise; control reads always ssh.

## 10. validation

- [x] 10.1 `cargo clippy -p thegn-svc -p thegn-core --lib` + host bins,
      `-D warnings` green; unit tests (classifiers, template, mosh argv) pass.
- [x] 10.2 **Live smoke PASSED** (ignored `tests/machine0_live.rs`,
      `MACHINE0_API_KEY`): create (dynamic size → cheapest, region default) → ssh
      `run_exec` → file round-trip → list → **devenv** (nix + /nix/store, best-effort
      `nix develop`) → **suspend** (peek confirms parked) → **resume** → exec →
      destroy — **no `machine0` binary**. VMs get a public SSH-reachable IP.
- [ ] 10.3 Pre-PR gate: `just ci`.
