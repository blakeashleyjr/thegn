# Sandbox

## Purpose

Each worktree's interactive process can run inside an isolation backend so that
untrusted or experimental work is contained, while the worktree itself stays on
the host so host-side git reads keep working. Backend selection degrades
gracefully across the available container/sandbox runtimes.

## Requirements

### Requirement: Graceful backend selection

The sandbox SHALL select an isolation backend by preference order podman -> docker -> bwrap -> none, MUST fall back to the next when a runtime is unavailable, and MUST fall back to `none` (run on the host) rather than failing to launch when no backend exists.

#### Scenario: Preferred runtime missing

- **WHEN** podman is not installed but docker is
- **THEN** the worktree process launches under docker

#### Scenario: No runtime available

- **WHEN** none of podman, docker, or bwrap is available
- **THEN** the process runs with backend `none` on the host and the worktree is
  still usable

### Requirement: Worktree stays on the host and is bind-mounted

A sandboxed worktree SHALL remain on the host filesystem and MUST be bind-mounted into the container at its real host path, so host-side git reads and the compositor continue to operate on the same files.

#### Scenario: Host git reads remain coherent

- **WHEN** a worktree process runs inside a container backend
- **THEN** the worktree is bind-mounted at its real path and git status/diff read
  from the host see the same working tree the sandboxed process edits

### Requirement: Sandboxing is per-worktree

Isolation SHALL be configurable per worktree and MUST NOT be a single global setting for the whole session.

#### Scenario: Mixed backends across worktrees

- **WHEN** two worktrees are open with different sandbox settings
- **THEN** each worktree's interactive process uses its own configured backend
  independently

### Requirement: Shared .git/config is mounted read-only inside the sandbox

A sandboxed worktree process SHALL see the shared `<git-common>/config` mounted read-only while objects, refs, index, and the per-worktree `worktrees/<name>/config` stay writable, so commits work but no sandboxed process can pollute the shared config.

#### Scenario: In-sandbox commit works

- **WHEN** a sandboxed process commits
- **THEN** the writes to objects/refs/index succeed

#### Scenario: In-sandbox shared-config write is refused

- **WHEN** a sandboxed process runs `git config`/`git remote add` against the
  shared config
- **THEN** the write fails by design

### Requirement: Per-worktree tunnel via a sidecar leaves the host untouched

A worktree MAY attach to its own overlay network through a per-worktree sidecar container whose network namespace the worktree joins (`--network container:<sidecar>`); thegn MUST NOT embed a tunnel datapath, and the host's networking (including any host `tailscaled`) MUST remain unchanged.

#### Scenario: Worktree egress is the tunnel

- **WHEN** a VPN is enabled for a worktree
- **THEN** the sidecar holds NET_ADMIN/TUN, the worktree's only egress is the
  tunnel, and the host network is untouched

#### Scenario: Sidecar torn down with the worktree

- **WHEN** the worktree closes
- **THEN** the `-szvpn` sidecar is removed and the ephemeral node de-registers

### Requirement: SealedTunnel profile has no direct host egress

The `SealedTunnel` profile SHALL apply the same lockdown as `sealed` but route egress through the tunnel, MUST degrade to `network=none` when no VPN is configured, and plain `sealed` MUST refuse a VPN.

#### Scenario: No VPN degrades to offline

- **WHEN** `SealedTunnel` is selected without a VPN configured
- **THEN** the worktree runs with `network=none`

#### Scenario: Plain sealed refuses a VPN

- **WHEN** a VPN is configured under plain `sealed`
- **THEN** it is refused

### Requirement: Tunnel failure never falls through to a less-isolated backend

When a tunnel fails to come up, the `on_error` policy SHALL govern the outcome and the `fail` setting MUST abort rather than launch the worktree with weaker isolation.

#### Scenario: on_error=fail aborts

- **WHEN** the tunnel fails to become ready and `on_error=fail`
- **THEN** the worktree does not launch with direct host egress

### Requirement: Resolve and inject the repo devShell env into worktree panes

When a worktree's repo exposes a flake `devShell` and `[sandbox] inject_devshell` is enabled, thegn SHALL resolve the devShell env on the host (`nix print-dev-env --json`), cache it by a `flake.lock`+`flake.nix` hash, and merge the exported variables into each worktree pane before the sandbox exec (PATH prepended, other vars set only if unset); a repo without `nix`/`devShell` MUST be a clean no-op.

#### Scenario: Flake repo gets the toolchain

- **WHEN** a worktree pane is spawned in a repo with a flake devShell
- **THEN** the pane's PATH includes the devShell tool directories

#### Scenario: Non-flake repo is a no-op

- **WHEN** a worktree pane is spawned in a repo with no flake devShell
- **THEN** no `nix` is invoked and the pane gets its ordinary environment

### Requirement: devShell resolution runs off the event loop

The devShell resolve SHALL run on a background thread that pulses the `TerminalWaker` and writes the cache, MUST NOT block pane spawn, and MUST NOT add a polling timeout; a cold pane applies the cache on a later spawn once warm.

#### Scenario: Cold resolve does not block

- **WHEN** the devShell cache is cold at pane spawn
- **THEN** the pane spawns immediately and the resolve proceeds off-loop, applying
  to subsequent spawns

### Requirement: Opt-in nix daemon mount

`[sandbox] nix_daemon` (default false) SHALL bind-mount the nix daemon socket and set `NIX_REMOTE=daemon` so full `nix develop`/`build` work inside the sandbox, and MUST warn and stay off when the host has no daemon socket.

#### Scenario: Enabled with a host daemon

- **WHEN** `nix_daemon` is true and the host daemon socket exists
- **THEN** the sandbox mounts the socket and nix operations work inside it

#### Scenario: No host daemon

- **WHEN** `nix_daemon` is true but no host daemon socket exists
- **THEN** thegn warns and leaves the mount off rather than half-wiring nix

### Requirement: A commodity VPS can serve as a managed sandbox provider

thegn SHALL support commodity-VPS vendors (Hetzner Cloud first) as managed
sandbox providers: an `[env.<name>.provider]` with a VPS kind SHALL create the
instance via the vendor's REST API (cloud-init user-data, the managed ssh key,
and thegn identity labels), wait until it is reachable over ssh, and run the
standard provisioning pipeline over an ssh exec/files transport that satisfies
the provider `files` capability. Panes and control-plane git/fs reads MUST
attach through a `thegn vps-ssh <name> --` self-bridge (the default
control/interactive prefix when no `exec_command` is configured) — no vendor
CLI is required. Secrets MUST NOT appear on any command line (host or remote);
provisioning env rides the ssh transport's stdin.

#### Scenario: Provisioning a VPS env yields an interactive pane

- **WHEN** a worktree resolves to a VPS provider env with its API token set and
  is provisioned
- **THEN** an instance is created via the vendor API with the managed key and
  thegn labels, the provisioning pipeline runs over ssh, and the pane
  attaches through the `vps-ssh` self-bridge

#### Scenario: The self-bridge is the default exec prefix

- **WHEN** a VPS provider env has no `exec_command` configured
- **THEN** the placement's control and interactive prefixes are the
  `thegn vps-ssh <resolved-id> --` self-bridge, and chrome git/fs reads and
  the persisted worktree location route through it

#### Scenario: A stale instance IP self-heals

- **WHEN** the attach bridge finds no usable ledger record for an instance
- **THEN** it re-resolves the IP via the vendor API and re-persists the record

### Requirement: VPS instances are leak-safe by construction

thegn SHALL write an intent record to the local instance ledger **before**
the create API call and finalize it (instance id, IP) after; every destroy
SHALL retry transient vendor errors and clear the record. A label-scoped reaper
(instances labeled as thegn-managed **and** created by this host) SHALL
destroy unledgered instances past a grace age, drop ledger records with no live
instance, and enforce the env's `max_lifetime_secs` ceiling. Creates MUST be
refused beyond the env's `max_instances` cap (default 5).

#### Scenario: A crash between create and record cannot leak

- **WHEN** the process dies after the create POST but before the ledger record
  is finalized
- **THEN** a later reaper pass finds the labeled instance without a ledger
  record and destroys it once it exceeds the grace age

#### Scenario: Another host's instances are never reaped

- **WHEN** two thegn hosts share one vendor account
- **THEN** the reaper only considers instances whose host label matches its own

#### Scenario: The instance cap refuses runaway creates

- **WHEN** the ledger already holds `max_instances` managed instances
- **THEN** a further create fails with an actionable error instead of minting
  another billing instance

### Requirement: VPS envs have no checkpoint semantics; speed comes from baked images

Because a stopped VPS still bills, thegn SHALL NOT expose
checkpoint/suspend semantics for VPS providers: the provider's `checkpoints`
capability is absent, the provisioning plan's checkpoint step is skipped, and
warm-pool spares (which record no checkpoint) MUST be destroyed — never
recycled in place — when stale or surplus. `thegn env image-bake` SHALL
build the speed substitute: a throwaway instance runs the repo-independent
provisioning prefix (Nix, direnv; docker via first-boot cloud-init), is
snapshotted and destroyed, and the printed `template = "snapshot:<id>"` makes
later provisions boot from the baked image.

#### Scenario: Stale VPS pool spares destroy instead of recycling

- **WHEN** a VPS warm-pool spare ages past the idle ceiling
- **THEN** the lifecycle destroys it (no restore-in-place path exists)

#### Scenario: image-bake never leaks its throwaway instance

- **WHEN** `thegn env image-bake` finishes — successfully or not
- **THEN** the bake instance is destroyed, and on success the snapshot template
  line is printed for the env config

### Requirement: DigitalOcean is a commodity-VPS provider kind

thegn SHALL support DigitalOcean as a `VpsKind` sharing the entire VPS core
(instance ledger, label/tag-scoped reaper, `thegn vps-ssh` self-bridge, image
bake). An `[env.<name>.provider]` with `provider = "digitalocean"` SHALL create
a droplet via the DigitalOcean REST API tagged as thegn-managed, register
the managed ssh key idempotently, wait until reachable over ssh, and run the
standard provisioning pipeline over the ssh exec/files transport. No vendor CLI
is required.

#### Scenario: A DigitalOcean env provisions over the shared VPS core

- **WHEN** a worktree resolves to a `provider = "digitalocean"` env with its API
  token set and is provisioned
- **THEN** a droplet is created via the DO API with the managed key and
  thegn tags, the pipeline runs over ssh, and the pane attaches through the
  `vps-ssh` self-bridge

#### Scenario: DigitalOcean reuses the VPS leak-safety machinery

- **WHEN** a DigitalOcean droplet is created
- **THEN** an intent record is ledgered before the create call and the
  label/tag-scoped reaper enforces `max_lifetime_secs` and destroys unledgered
  managed droplets — identical to the Hetzner path

### Requirement: Fly.io is a CLI-free managed sandbox provider

thegn SHALL support Fly.io as a first-class `Provider::Fly` (not a
`VpsKind`): an `[env.<name>.provider]` with `provider = "fly"` SHALL create an
app-per-sandbox Machine via the Fly Machines REST API, allocate a dedicated
IPv4 via the Fly GraphQL API, and make the machine reachable over the managed
ssh keypair (public IPv4 + guest sshd) so the provisioning pipeline runs over
the ssh exec/files transport that satisfies the provider `files` capability. No
Fly CLI (`flyctl`) is required; secrets MUST NOT appear on any command line.

#### Scenario: Provisioning a Fly env yields an interactive pane over ssh

- **WHEN** a worktree resolves to a `provider = "fly"` env with its API token
  set and is provisioned
- **THEN** an app + Machine is created via the Machines API with a dedicated
  IPv4, the pipeline runs over ssh to the guest sshd, and the pane attaches
  through the self-bridge — with no `flyctl` invocation

#### Scenario: A baked image boots a machine ready

- **WHEN** a Fly env sets `template = "image:<ref>"` for a published sandbox
  image
- **THEN** the created Machine boots from that image (toolchain + sshd already
  present) instead of building the toolchain on the cold start

### Requirement: Fly envs are scale-to-zero, not destroy-on-idle

thegn SHALL treat Fly as scale-to-zero because a stopped Fly Machine is
near-free (unlike a billing stopped VPS): an idle Fly sandbox is **stopped**
(not destroyed), and start/stop MUST poll the Machine's real state so a
still-transitioning machine is never treated as ready. The env's
`max_lifetime_secs` remains the hard destroy ceiling. A dedicated Fly reaper
(Fly is not a `VpsKind`) SHALL reconcile the `provider = "fly"` ledger records:
destroy a record past `max_lifetime_secs`, and reap a stale-`creating` record
whose create crashed between the intent write and finalize.

#### Scenario: An idle Fly sandbox stops instead of being destroyed

- **WHEN** a Fly sandbox goes idle under its lifetime ceiling
- **THEN** its Machine is stopped (near-free), not destroyed, and a later attach
  starts it again after confirming it reached the running state

#### Scenario: The Fly reaper enforces the spend ceiling

- **WHEN** a `provider = "fly"` ledger record exceeds `max_lifetime_secs`
- **THEN** the Fly reaper destroys the app/Machine (a running machine bills) and
  clears the record

#### Scenario: A crashed Fly create cannot leak

- **WHEN** a Fly `creating` ledger record is older than the stale-creating grace
  age (the create crashed before finalize)
- **THEN** the Fly reaper best-effort destroys the app and drops the record

### Requirement: Provider secrets resolve through a layered store

thegn SHALL resolve every provider token through a single `secret::resolve`
chokepoint that accepts a layered `SecretRef`: `keyring:<service>/<account>`
(OS keyring), `env:VAR`, `file:PATH` (a `0600` file), and a bare string treated
as `env:` for back-compat. A writer path SHALL persist a collected token —
preferring the OS keyring and falling back to a `0600` file under the config
dir — and return the ref to store in config. Resolution MUST degrade gracefully
(keyring → file → env) so a host with no Secret Service never wedges a launch,
and secrets MUST NOT be echoed or written into config in plaintext.

#### Scenario: A stored token launches a provider env without an exported var

- **WHEN** a token is stored via the writer path and its `SecretRef` is written
  into `[env.<name>.provider]`
- **THEN** a later provision resolves the token through `secret::resolve` and
  launches the env without the user exporting an environment variable

#### Scenario: Missing keyring falls back without wedging

- **WHEN** `secret::resolve` is asked for a `keyring:` ref on a host with no
  Secret Service
- **THEN** it degrades to the file/env layers (or returns none actionably)
  rather than blocking or crashing the launch

#### Scenario: Existing bare/env configs keep working

- **WHEN** an existing config names a token as a bare env-var (e.g.
  `api_key_env = "FLY_API_TOKEN"`)
- **THEN** `secret::resolve` treats it as `env:` and the env launches unchanged

### Requirement: Environments are authored without hand-editing TOML

thegn SHALL provide a write path that creates, edits, and removes
`[env.<name>]` / `[env.<name>.provider]` definitions with comments and
formatting preserved, plus a generic `config set <dotted.key> <value>`. Env
definitions SHALL be written only to global config; a repo `.thegn.toml` may
only _select_ an env (`env = "…"`), and the write path MUST refuse an env
definition in a repo scope (the trust-clamp model). A CLI (`thegn env
create`/`rm`/`test`, `config set`) SHALL back these operations and be usable
headlessly.

#### Scenario: `env create` writes config and stores the secret

- **WHEN** `thegn env create <name> --provider fly --token-file <path>` runs
- **THEN** the env is written to global config, the token is stored via the
  secret writer, and no secret is printed

#### Scenario: A repo file cannot define an env

- **WHEN** the write path is asked to define `[env.<name>]` in a repo
  `.thegn.toml`
- **THEN** it refuses, allowing only the `env = "…"` selection key

#### Scenario: `env test` verifies a token cheaply

- **WHEN** `thegn env test <name>` runs against a configured env
- **THEN** it builds the provider and performs a cheap `list()` call, reporting
  success or an actionable failure without provisioning anything

### Requirement: Environments are creatable and manageable from the TUI

thegn SHALL surface environment setup in the compositor: an "Add
environment" wizard (reached from the palette) that branches its fields by kind
(`local`/`ssh`/`fly`/`digitalocean`/`hetzner`/`daytona`), accepts a pasted
token, validates it off-loop, and on submit writes the env + stores the secret;
and a System-tab `Environments` panel section listing every configured `[env.*]`
with a token-status glyph and row actions to bind the env to the current
worktree, test it, remove it, and open the wizard. Off-loop validation MUST feed
back over the refresh channel and pulse the waker (no idle polling).

#### Scenario: Creating an env from the wizard binds it to a worktree

- **WHEN** the user opens the Add-environment wizard, picks a cloud kind, pastes
  a token, and submits with bind-to-current-worktree
- **THEN** the env is written, the token stored, and the current worktree is
  bound to that env

#### Scenario: The Environments panel row actions manage a live env

- **WHEN** the user selects an env row in the System ▸ Environments section
- **THEN** `enter` binds it to the current worktree, `t` tests it off-loop
  (status reported via a toast), `x` removes the env and forgets its secret, and
  `n` opens the wizard

#### Scenario: Panel token status reflects the secret store

- **WHEN** the Environments section renders a configured env
- **THEN** its glyph shows whether a token resolves (present) or is missing,
  computed off-loop during hydration

### Requirement: A host is provisioned once via a resumable state machine and reused by every sandbox on it

thegn SHALL model each container-capable machine as a host that walks a
provisioning state machine (connect → probe runtime → ensure base image by
digest → seed warm volumes → Ready), persisting only durable checkpoints so an
interrupted provision resumes from the last completed step rather than
restarting. A worktree sandbox targeting a `Ready` host MUST spawn without
re-running any host-level step, and a second `ensure_ready` on a Ready host
with a fresh probe MUST perform zero transfers and zero installs.

#### Scenario: First sandbox on a host pays host setup once

- **WHEN** a worktree materializes onto a host that has never been provisioned
- **THEN** the host walks connect/probe/image/volume steps with per-step
  progress shown in the worktree's loading splash, then the sandbox spawns

#### Scenario: Second worktree on the same host is a fast path

- **WHEN** a second worktree materializes onto a host already `Ready`
- **THEN** no host-level work runs (no transfer, no install) and the sandbox
  spawns directly with the digest-pinned image and warm volumes

#### Scenario: Interrupted provisioning resumes, not restarts

- **WHEN** the provisioning process is killed mid-step and `ensure_ready` runs
  again
- **THEN** provisioning resumes from the last durable checkpoint (a partial
  image transfer continues from its recorded byte offset)

### Requirement: Concurrent provisioning of one host is single-flight

thegn SHALL serialize provisioning per host: when N worktrees (or the CLI)
request a host that is mid-provision, later callers MUST await the same
in-flight run — each still receiving live step progress — rather than starting
a duplicate install or transfer.

#### Scenario: Two tabs await one provision

- **WHEN** two worktree tabs materialize onto the same unprovisioned host
  concurrently
- **THEN** exactly one provisioning run executes and both tabs' splashes show
  its progress until both sandboxes spawn

### Requirement: Installing a container runtime on a host requires explicit consent

thegn MUST NOT install software on a host without a per-host grant: a probe
finding no runtime SHALL proceed to installation only via config
(`install_runtime = "auto"`), an interactive confirmation, or an explicit CLI
flag; background provisioning (eager, warm pool) MUST never prompt and never
install. A declined or `"never"`-configured host fails with an actionable
message instead of installing.

#### Scenario: Interactive materialize asks before installing

- **WHEN** a focused worktree targets a host without podman/docker and
  `install_runtime = "ask"`
- **THEN** a confirmation modal names the host and the runtime before any
  install runs, and declining fails provisioning with the decline recorded

#### Scenario: Background paths defer instead of installing

- **WHEN** eager pre-provisioning or warm-pool reconcile encounters a host
  needing runtime installation
- **THEN** no prompt appears and nothing installs; the work is deferred to a
  focused materialize

### Requirement: Base images are delivered by the best available route and verified by digest before boot

thegn SHALL resolve the multi-arch base image to the target host's
per-architecture digest, skip delivery entirely when that digest is already in
the host's inventory, and otherwise deliver it by the best available strategy —
defaulting to a resumable registry-less transfer over the existing SSH channel,
with registry pull, skopeo copy, remote build, and provider-template lowering
as alternates. The delivered image digest MUST be verified before any sandbox
boots from it; a mismatch refuses to boot.

#### Scenario: Digest already present is a no-op

- **WHEN** `ensure_ready` runs against a host whose inventory already contains
  the per-arch image digest
- **THEN** no bytes are transferred and the image step completes immediately

#### Scenario: Registry unreachable falls back to transfer

- **WHEN** the preferred registry pull fails but the SSH transfer route is
  available
- **THEN** delivery falls back to the registry-less transfer with a status
  note, not a provisioning failure

#### Scenario: Digest mismatch refuses to boot

- **WHEN** a delivered image's digest does not match the resolved per-arch
  digest
- **THEN** the host fails with a "digest mismatch — refusing to boot" error and
  no sandbox starts from the image

### Requirement: Hosts are declared globally and referenced by envs

thegn SHALL read host definitions only from global configuration
(`[host.<name>]`); repo overlays MUST NOT be able to define hosts. An
`[env.<name>]` MAY reference a host by name so multiple envs share one host's
provisioning; an ssh env without a host reference SHALL keep today's behavior
via an implicit anonymous host whose install consent is `never`.

#### Scenario: Two envs share one host's setup

- **WHEN** two envs both set `host = "gpu-box"` and worktrees on each
  materialize
- **THEN** host provisioning runs once and both envs' sandboxes spawn from the
  same image and warm volumes

#### Scenario: Inline-ssh env keeps working without a host section

- **WHEN** an env has `[env.<name>.ssh]` connection details and no `host` key
- **THEN** sandboxes spawn as before, with no runtime installation ever
  attempted on that machine

### Requirement: Host state is inspectable and failures are actionable

thegn SHALL surface each host's state (state-machine position, runtime info,
inventory, last error) in the panel and via `thegn host list|status`, and
every failure MUST carry its step, message, and whether retry can succeed —
mapped to actionable UI text and CLI exit codes (0 ready, 1 fatal, 2
retryable). Retrying a failed host resumes from the failed step.

#### Scenario: Failure names the step and the remedy

- **WHEN** provisioning fails because installation was declined
- **THEN** the error surfaced in the panel/modal names the host, the step, and
  how to grant consent

#### Scenario: CLI provisions headlessly

- **WHEN** `thegn host provision <name>` runs in a terminal
- **THEN** each step prints progress (including transfer bytes) and the exit
  code distinguishes ready, fatal, and retryable outcomes

### Requirement: Hosts are addable without editing configuration

thegn SHALL let a user add a host by typing its target (`user@host[:port]`
or a dumbpipe ticket) in the new-worktree wizard's "+ add host…" row or via
`thegn host add`; the definition persists in the state DB, becomes a
selectable env immediately, and is shadowed by a declarative `[host.<name>]`
of the same name.

#### Scenario: Wizard add-host round trip

- **WHEN** the user picks "+ add host…" in the wizard and submits `user@box`
- **THEN** the host is persisted, the wizard re-opens with the new host
  selectable, and provisioning (with the consent flow) starts on selection

#### Scenario: Config shadows a DB-added host

- **WHEN** a `[host.<name>]` exists in config.toml with the same name as a
  DB-added host
- **THEN** the config definition wins and `thegn host add`/`rm` refuse to
  override it

### Requirement: Repo toolchains materialize automatically on hosts

thegn SHALL detect a repo's stack (nix flake/devenv/shell.nix, tool-version
pins, or plain language manifests including package.json, pyproject,
deno.json, build.sbt) and materialize a working toolchain inside the host
sandbox — the repo's own devshell when it has one, an explicitly pinned mise
setup when it declares versions, else a synthesized nix devshell warmed into
the host's shared /nix volume — with `[toolchain]` config controlling the
mode and per-language package sets.

#### Scenario: A python repo without nix gets a devshell

- **WHEN** a worktree of a repo with only `pyproject.toml` opens on a Ready
  host
- **THEN** a synthesized devshell (python + uv by default) is warmed and the
  pane opens inside it; a second worktree on the same host reuses the warm
  volume without rebuilding

#### Scenario: Explicit pins win over synthesis

- **WHEN** the repo carries `.tool-versions`/mise pins
- **THEN** the mise tier provisions those exact versions instead of the
  synthesized devshell

### Requirement: The personal layer applies to host-backed sandboxes

thegn SHALL apply the user's `[sandbox.home]` personal layer (dotfiles,
setup commands, agent CLIs + their logins, atuin) to host-backed sandboxes by
default, over the host's generic exec channel, honoring the existing shell
strategy knobs; personal-layer failures MUST be best-effort (warned, never
blocking the pane).

#### Scenario: Agent logins ride into a host sandbox

- **WHEN** a worktree provisions on a host with agents configured
- **THEN** the agents' host config/credential files are present in the
  container home and the shell uses the user's dotfiles per strategy

### Requirement: Sprite checkpoints are captured and spares recycle in place

thegn SHALL persist the checkpoint id created at the end of sprite
provisioning (base snapshot + pool spare rows) and SHALL recycle stale or
delete-released spares by restoring them to their checkpoint in place instead
of destroying and rebuilding, guarded by lockfile freshness, falling back to
destroy on any restore failure.

#### Scenario: A stale spare recycles in seconds

- **WHEN** the pool maintainer finds a ready spare past its idle TTL with a
  fresh checkpoint
- **THEN** the spare is restored to its checkpoint and returned to `ready`
  without a from-scratch rebuild

#### Scenario: Stale lockfile forces a rebuild

- **WHEN** the repo's flake.lock changed since the spare's checkpoint
- **THEN** the spare is destroyed and rebuilt (never restored stale)

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
- **THEN** the effective policy denies all egress (a universal DNS block)

### Requirement: Additive repo requests are trust-on-first-use gated

The system SHALL gate additive sandbox requests from a repo overlay (extra
mounts, volumes, `init_script`, `prepare`, `image`, `ports`, `gpu`,
`nix_daemon`): such a request MUST NOT be applied unless a matching approval has
been recorded. An unapproved additive request is surfaced as pending, not
applied, and the worktree still opens. Approval is matched by the request's
canonical form, so a later edit that changes the requested set re-prompts.

#### Scenario: An unapproved mount is not applied

- **WHEN** a repo overlay requests `mounts = ["/etc:/host-etc:ro"]` with no
  recorded approval
- **THEN** the mount is not bound and the request is surfaced as pending

#### Scenario: An approved request applies on the next launch

- **WHEN** the same requested set has been approved
- **THEN** the request is applied at the next worktree launch

### Requirement: A key's resolution is explainable

The system SHALL provide `thegn config explain <key>` reporting the effective
value, the trust layer that set it, and — for `sandbox.*` keys with a repo path
— the clamp trace (which requests were denied or are pending, and why).

#### Scenario: Explain shows why egress was clamped

- **WHEN** `thegn config explain sandbox.network --repo <path>` is run against
  a repo whose overlay requested `network = "host"`
- **THEN** the output shows the effective value, its origin layer, and the denial
  reason for the repo request
