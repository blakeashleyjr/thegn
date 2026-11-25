# Sandbox

## MODIFIED Requirements

### Requirement: Per-worktree tunnel via a sidecar leaves the host untouched

A worktree MAY attach to its own overlay network through a per-worktree sidecar container whose network namespace the worktree joins (`--network container:<sidecar>`); thegn MUST NOT embed a tunnel datapath, and the host's networking (including any host `tailscaled`) MUST remain unchanged.

#### Scenario: Worktree traffic is attached to the tunnel

- **WHEN** a VPN is enabled for a worktree
- **THEN** the sidecar holds NET_ADMIN/TUN, the worktree joins the sidecar's
  network namespace, and the host network is untouched; the tunnel is not an
  exclusive route, so thegn does not report the worktree's egress as enforced

#### Scenario: Sidecar torn down with the worktree

- **WHEN** the worktree closes
- **THEN** the `-tgvpn` sidecar is removed and the ephemeral node de-registers

### Requirement: SealedTunnel profile has no direct host egress

The `SealedTunnel` profile SHALL behave as `sealed` when no VPN is configured and
SHALL refuse to launch while a VPN resolves, until an exclusive tunnel route is
enforceable. Plain `sealed` MUST refuse a VPN.

#### Scenario: No VPN degrades to offline

- **WHEN** `SealedTunnel` is selected without a VPN configured
- **THEN** the worktree runs with `network=none`

#### Scenario: A configured VPN is refused

- **WHEN** `SealedTunnel` is selected and a VPN resolves for the worktree
- **THEN** the launch is refused, because no exclusive tunnel route is enforceable

#### Scenario: Plain sealed refuses a VPN

- **WHEN** a VPN is configured under plain `sealed`
- **THEN** it is refused

### Requirement: A repo `.thegn.*` overlay is a clamped request, not an override

The effective sandbox for a worktree SHALL be resolved by clamping the repo-root
`.thegn.{toml,yaml,yml,json}` `[sandbox]` overlay against the trusted base
(global config plus the active profile overlay). The repo layer, being the
least-trusted authorship layer, MAY only _request within_ the trusted bound: a
constraint field may tighten but never weaken, and a field the repo may not set
is dropped. Every denial MUST be surfaced (a log line, plus a deduped
notification and status on the launch path) and MUST NOT halt the worktree —
resolution continues with the clamped sandbox. The named-env `[env.<name>]
sandbox` overlay is globally defined (trusted) and applies unclamped on top of
the clamped repo base.

#### Scenario: A repo cannot disable the sandbox or choose a backend

- **WHEN** a repo overlay sets `enabled = false` or `backend = "host"`
- **THEN** the request is denied, the effective sandbox keeps the trusted values,
  and a denial is surfaced

#### Scenario: A repo may tighten a constraint

- **WHEN** the trusted network mode is `nat` and a repo overlay sets
  `network = "none"`
- **THEN** the effective network mode becomes `none` (a strict tightening)

#### Scenario: A repo cannot widen egress beyond the trusted ceiling

- **WHEN** the trusted `network_allow` is `["*.github.com"]` and a repo overlay
  requests `["api.github.com", "evil.com"]`
- **THEN** the effective allow-list is `["api.github.com"]` and the uncovered
  entry is denied

#### Scenario: An empty repo allow-list denies all egress

- **WHEN** a repo overlay sets `network_allow = []`
- **THEN** the effective policy lowers to `network = none`

### Requirement: A backend whose runtime was never verified says so

thegn SHALL distinguish backends whose commands have been checked against the
real runtime from those that have not, and MUST report the difference wherever a
backend's availability is shown. A backend with no liveness check is reached by a
PATH probe alone, so its `ready` state means only that a binary exists — thegn
MUST NOT let that stand as a claim the sandbox will work.

An unverified backend that is not reserved SHALL remain selectable when a user names
it explicitly, and MUST NOT appear in the default backend chain. A reserved backend is
never selected (see 'Reserved backends are never selected'). thegn MUST NOT guess a
runtime's verbs in order to clear the mark.

#### Scenario: Doctor marks an unverified backend

- **WHEN** `thegn doctor` reports a backend that has no liveness check and whose
  verbs were never tested against the real runtime
- **THEN** the row carries a caveat saying `ready` means the binary is on PATH
  rather than that panes will work, in addition to any state remedy

#### Scenario: The caveat does not displace the remedy

- **WHEN** an unverified backend is also not installed or not running
- **THEN** both the remedy for that state and the verification caveat are shown

#### Scenario: An unrunnable backend is not also called unverified

- **WHEN** an unverified backend cannot run on this OS at all
- **THEN** only its unsupported state is reported, because that decides the row
  regardless of verification

#### Scenario: Launching under one warns

- **WHEN** a worktree pane launches under an unverified backend
- **THEN** thegn warns that it was selected on PATH presence alone, and the
  launch proceeds

#### Scenario: A verified backend is unaffected

- **WHEN** a backend whose runtime has been verified is reported
- **THEN** no verification caveat appears and no warning is emitted

#### Scenario: A reserved backend is reported reserved, not unverified

- **WHEN** a user names a reserved kind such as `smol`
- **THEN** config reports it reserved and the launch resolves via the chain, and
  doctor does not list it as an unverified backend

## ADDED Requirements

### Requirement: Egress reporting never exceeds enforcement

thegn SHALL report a worktree's egress posture no more strongly than the code enforces it. The reported
kind is one of `enforce`, `translate`, `port-pinned`, `offline` or `unmanaged`. thegn MUST NOT report
`enforce` unless an egress datapath with a live lease, on a path the backend's enforcement manifest
declares and verifies, is the only route out; until such a datapath exists no backend or placement
reports `enforce`. `network = "none"` on a verified backend with its own network namespace SHALL report
`offline`. Allow and block lists without enforcement, `nat` and `host` networking, a VPN attachment and
unverified backends SHALL report `unmanaged`. A provider placement SHALL report `translate` only when its
kind lowers the lists to a provider policy API and reads them back, and `unmanaged` otherwise. The
enforcement matrix MUST describe hostname rules as advisory and MUST NOT describe them as enforced DNS
filtering. Where hostname rules are configured and nothing enforces them, thegn SHALL warn at launch, at
provision, in `thegn config validate` and in `thegn doctor`, and MUST NOT describe the rules as enforced egress.

#### Scenario: Lists on a local OCI backend are unmanaged

- **WHEN** a local podman or docker worktree has `network_allow` or `network_block` lists and `network = "nat"`
- **THEN** its egress is reported `unmanaged` and the matrix net cell reads `netns; rules advisory`

#### Scenario: Validate warns that lists are advisory

- **WHEN** the main or profile config sets `[sandbox] network_allow` or `network_block` with `network = "nat"` or `"host"`, and no `*` block entry
- **THEN** `thegn config validate` prints a warning naming the lists as advisory on local backends, and exits successfully when there are no other problems
- **AND** a config with `network = "none"`, no lists, or a `*` block entry prints no such warning

#### Scenario: Network none is offline

- **WHEN** `network = "none"` on a verified podman, docker, bwrap or systemd backend with its own network namespace
- **THEN** its egress is reported `offline`, never `enforce`

#### Scenario: A VPN attachment is unmanaged even with network none

- **WHEN** a VPN sidecar is attached to a worktree whose configured network is `none`
- **THEN** its egress is reported `unmanaged`, because the sidecar's network namespace is a route out

#### Scenario: Nothing reports enforce without a credited path

- **WHEN** egress is derived for any backend and placement before an egress datapath and enforcement manifest exist
- **THEN** the result is never `enforce`

#### Scenario: A Sprites placement translates

- **WHEN** egress is derived for a Sprites provider placement
- **THEN** it is reported `translate`

#### Scenario: Other provider placements are unmanaged

- **WHEN** egress is derived for any provider placement other than Sprites
- **THEN** it is reported `unmanaged`

#### Scenario: Hostname rules without enforcement warn

- **WHEN** a Daytona, Fly, VPS, machine0 or custom env has hostname allow or block lists
- **THEN** launch, provisioning and `thegn doctor` each warn that the lists are not enforced for that provider

### Requirement: Reserved backends are never selected

A sandbox backend kind that config accepts but the build does not implement SHALL be marked reserved, and a
reserved kind MUST NOT be selected as a runtime candidate. The kinds `smol` and `appcontainer` are reserved
in addition to `wsl`. A reserved value named in config SHALL produce a warning and resolve through the
backend chain, and strict validation SHALL reject it.

#### Scenario: A reserved backend falls back with a warning

- **WHEN** config sets `backend = "smol"`
- **THEN** config loading warns `sandbox backend "smol" is reserved` and the backend resolves through `backend_chain`

#### Scenario: Strict validation rejects a reserved backend

- **WHEN** `thegn config validate --strict` reads a config that names a reserved backend
- **THEN** it reports an error

#### Scenario: Doctor reports a reserved chain entry

- **WHEN** `backend_chain` contains a reserved kind
- **THEN** `thegn doctor` reports that entry as reserved rather than as unknown or absent

### Requirement: Apple cannot claim network none

The Apple `container` backend SHALL NOT be launched with `network = "none"`, because its CLI cannot express
an offline network. thegn MUST refuse the launch before creating the container, with the stable refusal id
`network-unexpressible` and a remedy naming a backend that can express it or `network = "nat"`.

#### Scenario: Apple with network none is refused before create

- **WHEN** a worktree resolves to the Apple backend with `network = "none"`
- **THEN** admission refuses with `network-unexpressible` and no container is created

### Requirement: ssh multiplexing degrades safely

thegn SHALL enable ssh connection multiplexing only where it can work. On Windows, and when the control
socket path would exceed the Unix socket path budget of 84 bytes, thegn MUST pass `ControlMaster=no` and
`ControlPath=none` instead of a multiplexing path, and SHALL warn once for the over-long path.

#### Scenario: Windows does not multiplex

- **WHEN** an ssh control call is built on Windows
- **THEN** it carries `ControlMaster=no` and `ControlPath=none`

#### Scenario: A long Unix path degrades

- **WHEN** the control socket path for a host would exceed 84 bytes on Unix
- **THEN** the call carries `ControlMaster=no` and `ControlPath=none`, and one warning is shown

### Requirement: Experimental placements are clamped on stable

On the stable release channel, thegn SHALL drop every environment whose placement is experimental under the
`Remote` feature: ssh and Kubernetes placements, in addition to the provider environments dropped under the
`Providers` feature.

#### Scenario: Kubernetes envs are dropped on stable

- **WHEN** the stable channel clamps a config that defines an env with a Kubernetes placement
- **THEN** that env is removed, as an ssh env is

### Requirement: Sealed sandboxes share no writable host cache

In the `sealed` and `sealed-tunnel` profiles thegn MUST NOT bind any shared host cache or toolchain
directory writable. The `.rustup` directory SHALL be read-only. Content-addressed download stores (the Cargo
registry cache, Go module downloads and the npm content cache) SHALL be read-only, layered over a private
per-worktree cache directory. Every other cache SHALL be a private per-worktree directory under the user
cache home. Private cache directories MUST be created at launch admission with mode 0700, and a failure to
create one MUST omit the mount with a warning rather than fall back to the shared host directory. The
`standard` and `open` profiles keep their existing cache mounts.

#### Scenario: Sealed binds no shared writable cache

- **WHEN** a sealed or sealed-tunnel spec is resolved on any backend
- **THEN** no mount whose host path is a shared host cache is writable

#### Scenario: Rustup is read-only in sealed

- **WHEN** a sealed worktree binds `.rustup`
- **THEN** the mount is read-only and a toolchain install inside the sandbox fails

#### Scenario: Two worktrees get different private caches

- **WHEN** two sealed worktrees resolve their caches
- **THEN** each gets its own private host directory for the same destination

#### Scenario: Standard caches are unchanged

- **WHEN** a standard or open spec is resolved
- **THEN** its cache mount list is identical to what it was before this requirement

### Requirement: Standard sandboxes cannot write host-executed agent configuration

In the `standard` and `open` profiles thegn MUST NOT bind `~/.keychain` or `~/.claude-profiles` writable.
Where thegn carves a coding agent's configuration directory writable for session state, it SHALL re-bind
every entry the host later executes or sources read-only inside that carve: the agent's hooks, settings,
commands, agents, plugins, skills, shell snapshots and MCP configuration. A listed entry that does not exist
SHALL be created once on the host as a valid empty placeholder (a directory empty, a JSON file `{}`, other
files empty) before it is pinned, for local placements only. A listed entry that is a symlink SHALL NOT be
pinned and SHALL be reported. The shared transcript store stays writable.

#### Scenario: Keychain and other profiles are not writable

- **WHEN** a standard or open spec is resolved
- **THEN** no writable mount exists for `~/.keychain` or `~/.claude-profiles`

#### Scenario: Agent hooks and settings are read-only inside the carve

- **WHEN** a standard pane binds the agent configuration directory writable
- **THEN** its hooks, settings, commands, agents, plugins, skills, shell snapshots and MCP configuration are re-bound read-only after that bind

#### Scenario: A missing entry is created and pinned

- **WHEN** a listed entry such as `settings.local.json` does not exist for a local placement
- **THEN** thegn creates a valid empty placeholder once, pins it read-only, and reports the creation

#### Scenario: A symlinked entry is reported

- **WHEN** a listed entry is a symlink
- **THEN** it is not pinned and is reported at launch and in `thegn doctor`
