# Design

## Why MCP client + ssh data plane (not a CLI shell-out)

machine0's documented programmatic surfaces are its remote **MCP** endpoint
(`https://app.machine0.io/mcp`, `x-api-key`) and a `machine0` CLI. The user
requirement is **no CLI dependency**. The REST/OpenAPI URL resolves to a
Mintlify placeholder, so MCP is the complete, honest programmatic surface. Two
documented facts shape the design:

1. **MCP `ssh_exec` is one-shot** ("No interactive commands or stdin", ≤300s,
   no PTY). A thegn worktree tab is an interactive terminal, so the pane cannot
   ride `tools/call` — it must use real ssh.
2. **Managed-key private material is never returned over MCP** (CLI-only), and
   managed-key VMs are the only ones `ssh_exec` accepts. So we **import thegn's
   own public key** via `ssh_key_create`, hold the private half, and drive the
   pane / exec / files / NixOS rebuild over our ssh — `ssh_exec` is unused.

Result: machine0 is a **VPS-shaped `RemoteProvider`** — MCP control plane, ssh
data plane. It reuses `vps::ssh_shim::SshShim` (exec/files) and returns
`ExecKind::Ssh`, exactly like `VpsProvider`/`FlyProvider`, so the provisioning
pipeline, panes, chrome reads, and warm-pool rebind are unchanged.

## MCP transport

`Mcp0Client` is a minimal JSON-RPC 2.0 HTTP client (the JSON-RPC types are
reused from `thegn_core::mcp::protocol`). It runs a best-effort, once
`initialize` handshake (capturing any `Mcp-Session-Id`; machine0 is documented
stateless, so a tool call never blocks on it) and then POSTs `tools/call`.
Responses are handled for both Streamable-HTTP shapes: a plain
`application/json` body, or a `text/event-stream` frame whose final `data:`
event carries the JSON-RPC response. Tool results unwrap to
`structuredContent` when present, else the first text content block (parsed as
JSON when it is JSON); `isError` becomes an `Err`. All shaping is pure and
unit-tested.

## Tool-name assumptions

The `vm_*` and `ssh_exec` names are confirmed from machine0's MCP docs. The
keys/images verbs (`ssh_key_create`, `ssh_key_list`, `image_create`,
`image_list`) are the documented category names and are isolated in a `tool`
const block for a one-line correction if a live `tools/list` differs. Parsers
tolerate common field/envelope variants (`{machines|vms|items:[…]}`, `id`/`name`,
`publicIp`/`ipv4`/`address`/nested `network`) so a minor schema difference does
not require code surgery.

## NixOS provisioning

No `provision` MCP tool exists (it is CLI-only). Instead, after the VM is
RUNNING + reachable, `create()` runs `nixos-rebuild switch --flake <ref>` over
ssh when `provision_flake` is set. A local `path#attr` ref is uploaded to the VM
(`/root/thegn-provision`) via the file plane and rebuilt from there; a flake URL
(`github:owner/repo#host`) is applied verbatim. Bounded by a `tokio::time`
timeout; a failure fails `create` loudly but leaves the VM for debugging.

## Decisions

- **restore = recreate-from-image** (machine0 images have no in-place restore):
  `vm_destroy` + `vm_create --image <snapshot>`. Stable sandbox name; new VM id.
- **scale-to-zero** via `vm_suspend`/`vm_start`; `provider_scale_to_zero` is the
  single source of truth mirrored by `ProviderCaps::scale_to_zero`.
- **Not** a `vps_provider_kind` (no vendor REST / ledger / reaper coupling) and
  **not** `wss_native_provider_kind` (pane rides ssh, not `thegn sprite-exec`).

## Open validation (live)

The interactive pane assumes `vm_get` exposes a directly-SSH-able public
address. If machine0 VMs are relay-only, a no-CLI interactive pane is not
achievable and a fallback (headless `ssh_exec`-only, or the CLI purely as the
pane PTY bridge) must be chosen with the user. Task 7.2 probes this.
