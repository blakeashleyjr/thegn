# Add hosts as first-class resources (fast remote OCI sandboxes)

## Summary

Model a **Host** — a machine that can run OCI containers, reached locally, over
SSH, over an iroh tunnel, or via a cloud provider — as a first-class resource
next to workspaces and worktrees. Each host walks a **resumable, idempotent
provisioning state machine** to `Ready` exactly once (connect → probe runtime →
optionally install it with consent → ensure the baked multi-arch base image by
digest → seed warm volumes), and per-worktree sandbox spawn becomes a cheap
fast path that assumes `Ready`. This moves the ~20-minute-class setup cost that
sprites and bare SSH remotes pay **per sandbox** today to **once per host**,
shared across every worktree that lands there.

## Impact

- **Config** — new global-only `[host.<name>]` section (reach, image override,
  install consent, delivery preferences, warm volumes); `[env.<name>]` gains a
  `host = "<name>"` reference so many envs share one host. Repo overlays cannot
  define hosts (select-only, like envs).
- **DB** — `user_version` bump to **28**: `hosts` (durable state machine
  checkpoints + consent + heartbeat), `host_inventory` (digest-keyed images and
  volume seeds, per-arch), `host_events` (forensic step trail).
- **Sandbox spawn** — a Ready host injects a digest-pinned image
  (`name@sha256:<per-arch-digest>`), named warm volumes, and the remote OCI URL
  into the existing `SandboxSpec` (`image`/`volumes`/`oci_host`); pinned
  `sandbox.rs` is unchanged.
- **UI/CLI** — hosts appear in the sidebar, a System-tab panel section with
  actions (provision/probe/retry/rm-cache/consent), tabbar placement-chip
  decoration, wizard host readiness badges, and a new `szhost host` subcommand.
- **tasks.md**: group AE (container provisioning) items 385 (CoW/base image),
  386 (prewarmed pool), 392 (image build cache), 394 (base image catalog);
  group J (remote access); group AB 355 (BYO image substitution).

## Rationale

superzej already has the pieces this composes: `Placement::{Local,Ssh,K8s,Provider}`
with multiplexed SSH control (`ssh_base` ControlMaster), a warm-spare pool with
an atomic `provisioning → ready → claimed` DB state machine, single-flight
provisioning gates (`provision_gate.rs`), an eventing model (progress channel +
`TerminalWaker`), and `SandboxSpec` fields for image/volumes/remote-daemon URL.
What is missing is the _host-scoped_ layer: today image pulls are "pull if
missing" with no digest pinning, every sandbox re-derives its environment, and
a bare remote re-pays nix/tool installation per worktree. Hoisting that work
into a host lifecycle with digest-deduped inventory makes re-provisioning an
`image exists` check and sandbox spawn near-instant — deployable anywhere that
can run podman/docker, with cloud providers lowered to the same trigger.

## Non-goals

- **Replacing named envs** — `[env.<name>]` remains the unit of selection;
  hosts are what envs _land on_. Inline-ssh envs without a host reference keep
  working via implicit anonymous hosts (consent = never).
- **A hosted registry** — the default image delivery is registry-less transfer
  over the existing SSH channel; ghcr/registry pull is a supported alternate,
  never a requirement (except Daytona snapshot registration, which is
  registry-shaped by that provider's design).
- **k8s placement changes** — k8s keeps today's path; host lowering for k8s is
  a possible follow-up.
- **cosign enforcement** — a signature-verify seam is designed in, default off.
