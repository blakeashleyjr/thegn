---
id: sandbox-backends
title: Sandbox backends and enforcement
parent: sandboxing
order: 1
---

# Sandbox backends and enforcement

Part of [[sandboxing]]. Which backend runs a worktree, what each one enforces, and the isolation floor.

## Backends

`[sandbox] backend = "auto"` walks the chain until one works:
`podman-rootless` → `podman-rootful` → `docker` → `apple` → `bwrap` →
`jobobject` (Windows). There is no host fallback: with no usable backend, a
sandboxed launch is refused under the default floor. Pin one to skip the
probing — `podman-rootless`, `podman-rootful`, `docker`, `bwrap`, `systemd`,
`apple`, `jobobject`, `none`. Reserved (accepted by config, never selected):
`smol`, `wsl`, `appcontainer`; a reserved value warns and resolves through
`backend_chain`, and `thegn config validate --strict` rejects it.

The outer `bwrap` and `systemd-run` launchers are resolved to absolute paths
from thegn's own `PATH`. If either is missing there, launch is refused rather
than resolving it from the pane environment. Loader variables such as `LD_*`
and `DYLD_*` are withheld from the outer launcher. Under `bwrap` they are set
again inside the sandbox; a `systemd-run` launch without `bwrap` drops them.

If the systemd backend cannot create a pane's protected environment file, the
pane still starts without those host-sourced secrets. The launch warning names
each omitted secret key, never its value; secrets are not put on the command
line.

For local rootless Podman, the first thegn command is the `version` liveness
probe, before container creation or the events client. The `podman events`
client runs in its own process group, which thegn kills at shutdown and on
stream end, reaping it when it exits within the shutdown deadline.
A surviving `pause` process belongs to rootless Podman's user namespace and
may outlive thegn by design.

One chain serves every OS: each OS-native entry is probed only on its own OS.

> **NixOS tip:** set `backend = "bwrap"` to skip the podman probes and get
> instant panes.

> **macOS tip:** `apple` is Apple's `container` CLI. Each container gets its
> own Linux VM, so it is the strongest isolation any backend here offers.
> Without `apple`, podman, or docker, a Mac has no backend meeting the default
> floor, so sandboxed panes are refused. Opt out with
> `isolation_floor = "off"`, `backend = "none"`, `enabled = false`, or run on
> host; `bwrap`/`systemd` are Linux-only.
>
> apple: `network = "host"` is refused with `network-unexpressible` — Apple's
> `container` CLI has no host network at all; use `nat`. `network = "none"`
> (and therefore `sealed`) is refused too: Apple can create an internal
> network, but it still reaches services bound on your Mac, so it is not
> offline. Use podman or docker for offline or sealed worktrees on a Mac.

> **Platform status:** the public alpha supports Linux on x86_64, where
> `bwrap`, `systemd`, podman and docker are the backends to use. The `apple`
> backend and macOS are best-effort; the Windows `jobobject` and `wsl` entries
> are not supported in this alpha.

`thegn doctor` reports which backends this machine actually has.

## Enforcement matrix

`thegn doctor` renders one **enforcement matrix** for this host: per backend,
what it actually enforces —

| Cell      | What it means                                                                                                                                                                                                    |
| --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fs`      | filesystem isolation: a separate root, unit-level protection, or the host fs                                                                                                                                     |
| `net`     | `netns; rules advisory` (own namespace: `network = "none"` is enforced and reported offline; allow/block lists are not enforced), `only when network=none`, `none (host net)`, or `unverified (no none)` (apple) |
| `ceiling` | resource cap strength: hard cgroup/VM, soft `nice`, deferred, or none                                                                                                                                            |
| `scoping` | process-tree scoping: engine lifecycle, pid namespace, unit, job object, pgid                                                                                                                                    |
| `class`   | the honest isolation class (below) — what would have to fail for an escape                                                                                                                                       |

Every cell is **derived** from the same predicates the resolver uses, so the
matrix can never disagree with what actually launches. The ceiling cell reflects
what is _measured_ on this host — a machine without cgroup cpu delegation shows a
soft ceiling for the host-toolchain backends, not a hard one. Backends thegn's
verbs were never checked against a real install are flagged `(unverified)`.
When `systemd-run` and cgroup CPU delegation are available but no systemd user
manager is reachable, the ceiling uses soft `nice` or no cap if `nice` is
unavailable. With a CPU cap configured, `thegn doctor` prints a `cpu cap note`;
`thegn doctor --json` includes the same text as `cpu_scope_note`.
When the soft ceiling is in use, its label names missing hard-cap prerequisites:
`systemd-run not found`, `no cgroup v2 cpu delegation`, and/or `no reachable
systemd user manager`. `thegn doctor --json` reports the same reason as
`cpu_soft_reason` (null when no soft cap applies).

`thegn doctor` also lists sandbox execution routes such as worktree panes,
queue agents, tests and gates, hooks, language services, and host extensions,
with each route's admission status. `handle` means the launch carries sandbox
admission; `admitted-no-handle` means admission passes without carrying the
handle, and `partial` means only some spawns carry it. `own-gate` means a
separate floor or sealed gate; `host-capped` means host execution under the
shared resource ceiling; and `host-trust` means host execution on user trust.
`thegn doctor --json` includes the table as `sandbox.admission_routes`, with
`id`, `route`, `status`, and `path` fields.

When `[[pipeline.stages]]` are configured, the text sandbox report ends with
a `worker` row for the current worktree (or says there is nothing to probe
outside a Git worktree). `ready` means the resolved sandbox lets stage workers
write the worktree and Git metadata and reach the current `thegn` binary.
`NOT READY` names the first failed prerequisite, such as a missing backend,
unwritable mount or replaced binary. `NOT PROVEN` means the platform or
backend is unsupported by this check; it is not a pass. The row is absent
from `--json` and describes the current worktree's sandbox composition, not
a live agent run. The check supports bwrap, podman and docker.
A stage whose agent relocates its credential home (for example `CODEX_HOME`) is also checked: a missing home directory reports
`provider-home-unavailable`.

Queue agents (merge-conflict, gate-failure and PR-queue handoffs) run through
the same check at dispatch time: when the composed sandbox cannot write the
worktree or Git common directory, the agent is not started, the queue entry is
held rather than counted as a failed attempt, and the hold reason names the
readiness class (`cannot-write-worktree` or `cannot-write-git-metadata`) and
the backend.

Agent pipeline runs make the same check after composing the sandbox. If it
cannot write the worktree, the Git common directory or the agent's relocated
credential home, or if the `thegn` binary was replaced since launch, the stage
does not start and the run is refused with the readiness class
(`cannot-write-worktree`, `cannot-write-git-metadata`, `provider-home-unavailable`
or `callback-unavailable`) and backend. The `worker` row in `thegn doctor` is
the same check.

The honest **isolation class**, weakest to strongest:

| Class              | Escape needs…                               | Backends                                             |
| ------------------ | ------------------------------------------- | ---------------------------------------------------- |
| `host-process`     | nothing — no boundary                       | `none`, and the Windows Job Object (scoping only)    |
| `shared-kernel`    | a host-kernel exploit in an allowed syscall | podman/docker/bwrap/systemd                          |
| `userspace-kernel` | a gVisor Sentry bug (`oci_runtime="runsc"`) | OCI + runsc                                          |
| `guest-kernel`     | a VMM/KVM bug                               | `apple`, `oci_runtime="krun"`, macOS VM-mediated OCI |

A Windows Job Object is process-tree lifetime + resource scoping with **no**
filesystem or network boundary, so it is honestly `host-process`, never a
container — it satisfies no floor at `shared-kernel` or above.

## Remote and macOS runtimes

Only a local placement on a Linux host probes the runtime binary and
`/dev/kvm`. A remote placement (ssh, a provider sandbox) or a macOS host keeps
the runtime you requested, because the local machine cannot tell whether it
exists there. On a local Linux host, `oci_runtime = "krun"` needs `/dev/kvm`
to open read-write; otherwise it degrades and `thegn doctor` reports the
resulting isolation class.

## Isolation floor

`backend_chain` expresses a _preference_; the isolation floor is a _demand_.

```toml
[sandbox]
isolation_floor = "guest-kernel"   # off | shared-kernel | userspace-kernel | guest-kernel; unset = default
on_floor_miss   = "fail"           # degrade | fail; applies only to an explicit floor
```

With `[sandbox] enabled = true` and no explicit floor, the floor is
`shared-kernel` and a miss always refuses. `enabled = false`,
`backend = "none"`, and the run-on-host pin bypass only that default floor.
An explicit floor from any layer always wins, even over those settings. A
`backend_chain` that lists `host` is not a host opt-out.

The floor is compared over the **honest class** of what the launch actually
enters, after backend selection and any `oci_runtime` degrade — so a `krun` that
fell back to the daemon default counts as `shared-kernel`, and a macOS local OCI
container counts as `guest-kernel`. On a miss, `degrade` launches with the
worktree marked degraded and a warning; `fail` refuses to launch and names the
floor, available class, and how to satisfy it. The refusal names the opt-outs:
`isolation_floor = "off"`; with no explicit floor, `enabled = false`,
`backend = "none"`, or `run on host` also opts out; for an explicit floor,
`on_floor_miss = "degrade"` opts out of refusal.
`thegn doctor` prints the effective floor, miss policy, and source, for example
`floor         shared-kernel (on miss: fail; default)`, or the reason the
default floor was not applied. It also prints `containment` and `file access`
lines, including a line such as
`requested <scope> — NOT ENFORCED on <backend>: <note>` when file access is not
enforced. It prints separate `requested`, `resolved`, and `enforced`
isolation lines, and `thegn doctor --json` includes the same layers in an
`isolation` object. The floor also applies to standalone terminal tabs. If a
terminal requests a backend with no runtime, it is refused under the default
floor or an explicit `fail` floor; an explicit floor with `degrade` launches it
labelled degraded. Explicit `host`/`none`/empty backend choices and ssh/mosh
terminals are unchanged. A managed provider
placement is out of scope (reported `provider-managed`, never counted as a
tier). A repo-root `.thegn.toml` may only **raise** the floor, never lower it.
With `agent_sandbox` on, a queue agent is held to at least the `[sandbox]`
isolation floor in force (default: shared-kernel, refusing on a miss). The
queue floor (`agent_isolation_floor`) can raise that requirement, but
`degrade` cannot weaken it. With `agent_sandbox = false`, a fail-closed floor
still holds the entry as an infrastructure failure and never marks the branch
failed. Set `[sandbox] isolation_floor = "off"` or disable the sandbox to run
queue agents uncontained.

Queue agents (merge and PR fixers, CI autofix, headless review and review
tasks) run credential-free, like autopilot. Their environment and sandbox
passthrough drop credential-shaped variables plus `GH_CONFIG_DIR`, `GNUPGHOME`,
`GIT_SSH_COMMAND`, `GPG_TTY`, `DBUS_SESSION_BUS_ADDRESS` and inherited `THEGN_*`
variables (the resolver's `THEGN_SANDBOX` marker is retained). Autonomous
sandbox specs also lose SSH identity mounts and mounts at
or below `~/.ssh`, `~/.gnupg` and `~/.config/gh`; Git credential helpers are
reset. Autonomous agents (queue agents and autopilot) see the repository's
`.git/hooks` and the worktree's `config.worktree` read-only, so they cannot plant
a hook that host-side git later runs; interactive panes are unchanged.
`XDG_RUNTIME_DIR` and `GIT_CONFIG_GLOBAL` are kept for launcher plumbing
and commit identity. A broad `[sandbox] mounts` entry that exposes the whole
home still exposes these directories; they are not masked inside a broad home
mount. With `agent_sandbox` on, a queue agent whose sandbox cannot be
established is held; it never falls back to the host. Interactive panes are
unchanged.

### Git write policy

`[sandbox.git] mode` describes what a sandboxed pane may write in the
repository's git directory: `read-only`, `commit-only` (the default: the
object store, the worktree's own metadata and the lane branch's ref directory
such as `refs/heads/tg/`, with `main`, tags, hooks and config kept
read-only), `push-lane` or `full`. `push_refs` and `deny_refs` take ref globs
with a trailing `/*` (one level) or `/**` (any depth), and the merge target
and default branches are always denied. Only `refs/heads/` branches are
accepted back from a sandbox. Set it globally or per `[env.<name>.sandbox.git]`; a
repository's `.thegn.toml` cannot set it. The keys are validated today; the
sandbox mounts that enforce them arrive in a later release.

Sandboxed and remote agents get no GitHub token by default: credentials stay
with thegn. With the default `credential = "none"`, `GH_TOKEN` and
`GITHUB_TOKEN` are not passed into sandboxes or provider provisioning even
though the default `[sandbox] env_passthrough` lists them (nor are
`GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, `GITLAB_TOKEN` and
`GL_TOKEN`), and provisioning
does not install a git token credential helper. Only `mode = "full"` with
`credential = "repo-token"` or `"user-token"` passes the host token through.
`ANTHROPIC_API_KEY` and other agent credentials are unaffected.
`credential = "deploy-key"` with `deploy_key = "~/.ssh/deploy"` (local
placements only) binds that key read-only into the sandbox and sets
`GIT_SSH_COMMAND` to use it; the key must be a regular file you own that is
not group or world readable, otherwise the sandbox gets no credential and a
warning is logged. A `GIT_SSH_COMMAND` you pass through wins. `github-app`
is not implemented yet. Only
environment variables are filtered: a `gh` or `glab` config (with its stored
token) that reaches the sandbox through `[sandbox.home]` dotfiles is not.
