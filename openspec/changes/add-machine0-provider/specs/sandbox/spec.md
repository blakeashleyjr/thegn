# Sandbox

## ADDED Requirements

### Requirement: machine0 is an MCP-native managed-sandbox provider

thegn SHALL support machine0 (machine0.io) as a first-class
`Provider::Machine0` whose control plane is machine0's remote **MCP** endpoint
and whose data plane is ssh. An `[env.<name>.provider]` with
`provider = "machine0"` SHALL, using only MCP `tools/call` over HTTP (auth via
an `x-api-key` header — **no `machine0` CLI binary**), create a VM
(`vm_create`), poll it to RUNNING (`vm_get_by_name`), and make it reachable over
thegn's managed ssh keypair (imported via `ssh_key_create`), so the standard
provisioning pipeline runs over the ssh exec/files transport that satisfies the
provider `files` capability. Secrets MUST NOT appear on any command line (the
api key rides the `x-api-key` header; the ssh private key stays host-side).

#### Scenario: Provisioning a machine0 env yields an interactive pane over ssh

- **WHEN** a worktree resolves to a `provider = "machine0"` env with its
  `MACHINE0_API_KEY` set and is provisioned
- **THEN** a VM is created via the MCP `vm_create` tool with thegn's imported
  ssh key, the pipeline runs over ssh to the VM, and the pane attaches over ssh
  (`ExecKind::Ssh`) — with no `machine0` binary invoked

#### Scenario: The MCP client tolerates both HTTP response shapes

- **WHEN** the machine0 MCP endpoint answers a `tools/call` with either an
  `application/json` body or a one-shot `text/event-stream` frame
- **THEN** thegn decodes the JSON-RPC response from either, unwraps the tool
  result (`structuredContent` or the text content block), and surfaces an
  `isError` result or a JSON-RPC error as a failure

#### Scenario: Destroy is idempotent

- **WHEN** a machine0 sandbox is destroyed but the VM no longer exists
- **THEN** the destroy resolves the VM by name, treats a not-found as success,
  and never leaks or errors

### Requirement: machine0 provisions NixOS VMs from a Nix flake

thegn SHALL support NixOS on machine0: an env MAY set
`template = "nixos-25-11-loaded"` (or any machine0 NixOS image) and a
`provision_flake` ref. After the VM is RUNNING and reachable, thegn SHALL apply
the flake with `nixos-rebuild switch --flake <ref>` over ssh (there is no
provider tool for provisioning). A local `path#attr` ref SHALL be uploaded to the
VM first and rebuilt from the uploaded directory; a flake URL SHALL be applied
verbatim. A failed apply SHALL fail create loudly and leave the VM for debugging.

#### Scenario: A NixOS env applies its flake on create

- **WHEN** a `provider = "machine0"` env with a NixOS `template` and a
  `provision_flake` is provisioned
- **THEN** after the VM reaches RUNNING, thegn runs `nixos-rebuild switch
  --flake <ref>` over ssh and only returns the sandbox handle once the apply
  succeeds

#### Scenario: A NixOS env without a provision flake skips the rebuild

- **WHEN** `provision_flake` is empty
- **THEN** create performs no `nixos-rebuild` step and returns the running VM

### Requirement: machine0 supports snapshots and scale-to-zero

thegn SHALL expose machine0's image snapshots as provider checkpoints
(`caps().checkpoints`): `checkpoint` creates an image (`image_create`),
`list_checkpoints` lists images (`image_list`), and `restore` recreates the VM
from a saved image (`vm_destroy` + `vm_create --image` — machine0 has no
in-place restore). thegn SHALL treat machine0 as **scale-to-zero**: an idle
sandbox is suspended (`vm_suspend`, billed only for storage), not destroyed, and
resumed (`vm_start`) on claim, with `provider_scale_to_zero("machine0")` as the
single source of truth. machine0 is NOT a commodity-VPS kind and NOT a
WSS-native exec provider.

#### Scenario: A checkpoint snapshots the VM and restore recreates it

- **WHEN** `thegn env snapshot` runs against a machine0 sandbox and `env
  restore <image>` is later invoked
- **THEN** an `image_create` captures the VM, and restore destroys the VM and
  recreates it from the saved image under the same sandbox name

#### Scenario: An idle machine0 sandbox suspends instead of being destroyed

- **WHEN** a machine0 sandbox is idle past the warm-pool TTL
- **THEN** it is suspended (`vm_suspend`) and resumed (`vm_start`) on next
  claim, because `provider_scale_to_zero` classifies machine0 as scale-to-zero

### Requirement: machine0 panes and chrome reads reach the VM over ssh

thegn SHALL route a machine0 env's interactive pane AND its chrome git/fs
control reads to the VM through a `machine0-ssh` self-bridge (the role `vps-ssh`
plays for VPS), so `control_command_template` yields a non-empty prefix and the
persisted `GitLoc::Provider` resolves into the VM rather than the host. The
bridge SHALL resolve the VM address + ssh user via the provider (`vm_get`),
waking a suspended VM only for an interactive attach (resume-on-open) and never
for a control read (which serves cached state when the VM is parked).

#### Scenario: Opening a machine0 worktree attaches a shell on the VM

- **WHEN** a worktree bound to a `provider = "machine0"` env opens its pane
- **THEN** `thegn machine0-ssh <id>` resolves the VM's IP + ssh user and execs an
  interactive shell ON THE VM (waking it first if it was parked), not on the host

#### Scenario: An idle machine0 VM is explicitly parked

- **WHEN** the warm/idle reconcile decides to suspend an idle machine0 worktree
- **THEN** thegn calls `Provider::suspend` (`vm_suspend`) — because machine0 does
  not self-suspend (`provider_self_suspends` is false) — before dropping the
  bridge, and reopening the worktree resumes it via the pane bridge

### Requirement: machine0 interactive panes default to mosh with ssh fallback

thegn SHALL default the machine0 (and provider-over-ssh) interactive pane
transport to **mosh** (`[env.<name>.provider] transport`, default `mosh`),
emitting `mosh --ssh="<ssh opts>" <user>@<ip>` when a local mosh client and the
VM's `mosh-server` are both present, and falling back to plain ssh otherwise.
The control plane MUST remain ssh (mosh cannot pipe non-interactive commands).

#### Scenario: A pane uses mosh when available, ssh otherwise

- **WHEN** a machine0 pane opens with the default `transport = "mosh"`
- **THEN** it attaches over mosh if the VM has `mosh-server` (and the host has
  `mosh`), otherwise it transparently falls back to ssh — never failing the pane
