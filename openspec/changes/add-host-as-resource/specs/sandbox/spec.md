# Sandbox

## ADDED Requirements

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
