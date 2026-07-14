# Add machine0 as an MCP-native VM provider (NixOS)

## Summary

Extend the managed-sandbox provider seam with **machine0** (machine0.io), a
cloud-VM platform, driven **entirely over machine0's remote MCP endpoint** —
with **no dependency on the `machine0` CLI binary**. This is the first place
thegn acts as an **MCP client** (elsewhere it only serves MCP or passes declared
servers to the agent).

The provider is **VPS-shaped**: the control plane is MCP, the data plane is ssh.

- **Control plane → MCP.** thegn-svc gains a minimal JSON-RPC MCP-over-HTTP
  client (`machine0/mcp.rs`) that POSTs `tools/call` to `https://app.machine0.io/mcp`
  with an `x-api-key` header (handling both `application/json` and one-shot
  `text/event-stream` responses). `Machine0Provider` calls the documented tools:
  `vm_create` / `vm_get_by_name` / `vm_list` / `vm_destroy` / `vm_start` /
  `vm_suspend` (lifecycle), `image_create` / `image_list` (snapshots),
  `ssh_key_create` / `ssh_key_list` (import thegn's managed key), `sizes`.
- **Data plane → ssh.** machine0's MCP `ssh_exec` tool is one-shot with no PTY,
  so the interactive pane, `run_exec`, file sync, and the NixOS flake apply ride
  **plain ssh with thegn's managed keypair** (`ExecKind::Ssh`) — reusing the VPS
  ssh-shim (`vps/ssh_shim.rs`). We import _our own_ public key (managed-key
  private material is never returned over MCP), so we hold the private half.
- **NixOS.** A machine0 NixOS image (`template = "nixos-25-11-loaded"`) plus a
  new `provision_flake` config key: after create, thegn applies the flake via
  `nixos-rebuild switch --flake <ref>` over ssh (there is no provider tool for
  it). A local `path#attr` is uploaded first; a flake URL is used verbatim.

The provider reuses the transport-agnostic provisioning pipeline gated on
`caps().files`, so clone → nix → dotfiles → agents → parity run unchanged, and
panes/chrome-reads/persisted-location route through the ssh transport.

## Impact

- tasks.md: **AE** (provider backends) — the next backend in the lineage of
  **AE 749** (`add-vps-providers`, the VPS core this ssh data plane reuses) and
  **AE 756** (`add-do-fly-providers`, the ssh-transport `Provider` precedent).
  Complements the **mcp-servers** capability (AL) by making thegn-svc an MCP
  _client_ for the first time.
- **thegn-svc** — new `machine0` module (`machine0/mcp.rs` MCP-over-HTTP client,
  `machine0/mod.rs` `Machine0Provider`) with a `Provider::Machine0` variant
  (caps: `files`, `checkpoints`, `scale_to_zero`). Reuses `vps::ssh_shim::SshShim`
  for the data plane.
- **thegn-core** — `EnvProviderConfig` gains `provision_flake`;
  `provider_scale_to_zero()` returns true for `machine0` (idle `vm_suspend`, not
  destroy). `vps_provider_kind` / `wss_native_provider_kind` unchanged.
- **thegn-host** — `provider_factory.rs` gains `machine0_provider_for`; the
  `machine0` arms in `provider_for_named`, `cmd/env.rs` `create_env` +
  `api_provider`.
- **No DB schema change** — git/`vm_list` is the source of truth; the ssh-shim
  reuses the per-instance known-hosts registry.

## Rationale

machine0's fully-documented, complete programmatic surface is its MCP endpoint
(the REST/OpenAPI is a Mintlify placeholder), so an MCP client is the honest,
CLI-free integration. But MCP `ssh_exec` cannot back an interactive terminal
(one-shot, no PTY) and managed-key private material is CLI-only, so the pane must
ride real ssh with an imported key — exactly the VPS/Fly transport. machine0 is
therefore a VPS-shaped `RemoteProvider` whose control plane happens to be MCP,
satisfying the same `files` capability over the same managed-ssh transport, so
the pipeline, panes, chrome reads, and warm-pool rebind all run through existing
code paths — while adding native image snapshots (checkpoints), `vm_suspend`
scale-to-zero, and first-class NixOS provisioning.

## Non-goals / decisions

- **Provider only** — no `[mcp_servers.machine0]` registration for the agent
  layer (that is additive and orthogonal).
- **`restore` = recreate-from-image** — machine0 images have no in-place restore,
  so `restore` is `vm_destroy` + `vm_create --image <snapshot>` (recreate; the
  underlying VM id changes, the stable sandbox name does not).
- **Managed-host autoscale template** (`ManagedTemplate.provider`) is deferred —
  the provider-env path is the deliverable.
- **Reachability assumption** — the interactive pane requires that `vm_get`
  returns a directly-SSH-able public address; the provider parses a tolerant set
  of address fields. Confirming this against a live account is the first
  verification step.
