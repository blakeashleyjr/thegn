---
id: sandboxing
title: Sandboxing
parent: workflows
order: 3
contexts: [panel:sandbox, panel:environments]
actions: [warm-pool-increment, warm-pool-decrement]
---

# Sandboxing

Each worktree's interactive process — often a coding agent — can run in a
container while the worktree itself **stays on the host**, bind-mounted at
its real path. That is the whole trick: host-side git keeps working, so
the [[panel]] diff and the [[sidebar]] state are live and unsandboxed
while the process is contained.

## Hardening profiles

`[sandbox] profile` picks a bundle of isolation knobs. Set it globally,
per repo via a `.thegn.toml` overlay, or per run with
`THEGN_SANDBOX_PROFILE`.

| Profile    | What it does                                                                                   |
| ---------- | ---------------------------------------------------------------------------------------------- |
| `open`     | no hardening — the escape hatch                                                                |
| `hardened` | **default.** read-only root, no-new-privileges, a process cap; network and capabilities intact |
| `sealed`   | full lockdown: `network=none`, drop ALL caps, tighter pids cap. For untrusted work             |

Under `hardened` or `sealed`, **everything outside the worktree is
read-only — including your `$HOME`**, so a sandboxed agent cannot `cd`
out and modify host files.

Writable carve-outs: the worktree and its git dir, build caches, `/tmp`,
and a narrow set of `$HOME` paths for shell state (`~/tmp`,
`~/.local/state`, `~/.local/share`, `~/.zsh_history`, `~/.bash_history`).
Agent config dirs (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`, or the `~/.claude`
/ `~/.codex` defaults) are writable for every non-sealed pane, so an
agent CLI you run by hand can persist its state instead of dying on a
read-only filesystem.

> `~/.keychain` is carved writable for `hardened` panes but **not** for
> `sealed` ones: it holds scripts your host login shells later _source_,
> so a writable copy from a sealed sandbox would be a persistence vector.

## Backends

`[sandbox] backend = "auto"` walks the chain until one works: `podman` →
`docker` → `apple` → `bwrap` → `none`. Pin one to skip the probing —
`podman-rootless`, `podman-rootful`, `docker`, `bwrap`, `systemd`,
`apple`, `wsl`, `none`.

One chain serves every OS: each OS-native entry is probed only on its own OS.

> **NixOS tip:** set `backend = "bwrap"` to skip the podman probes and get
> instant panes.

> **macOS tip:** `apple` is Apple's `container` CLI. Each container gets its
> own Linux VM, so it is the strongest isolation any backend here offers.
> Without it — and without podman/docker — a Mac lands on `none` and panes
> run directly on the host; `bwrap`/`systemd` are Linux-only.

`thegn doctor` reports which backends this machine actually has.

## Devcontainers

With the default `auto` mode, thegn discovers a repo's
`.devcontainer/devcontainer.json` or `.devcontainer.json`, parses JSONC, and
maps its supported subset onto the existing sandbox. To opt out globally or in
a trusted overlay:

```toml
[sandbox]
devcontainer = "off" # "auto" (default) | "off"
```

If neither primary file exists, thegn also discovers
`.devcontainer/<name>/devcontainer.json`. One variant is unambiguous; multiple
variants need a top-level selector in the repo's `.thegn.toml`:

```toml
devcontainer = "gpu"

[sandbox]
devcontainer = "auto"
```

The selector only picks a file. It does not approve anything in that file.
Review pending category requests and their ids with `thegn repo trust .`, then
approve one explicitly with `thegn repo trust . --approve <id>`.

### Security boundary

A repo JSON file is untrusted, just like any other cloned executable content.
thegn records trust-on-first-use approval separately for its image, build,
compose, mounts, ports, lifecycle, and features categories. An unapproved
category stays pending and is not applied; the worktree still opens. Editing a
request changes its canonical form and requires approval again.

Literal `containerEnv` and `remoteEnv` values do not prompt, but host-variable
substitution is clamped: `${localEnv:NAME}` is available only when `NAME` is in
the effective `[sandbox] env_passthrough` list. A blocked name becomes empty
and is reported without exposing its value.

Some repo requests are never eligible for approval. `privileged`, `capAdd`,
`securityOpt`, `runArgs`, and `init` could weaken the trusted sandbox, so thegn
always refuses them and names the reason. Recognized fields that thegn does not
apply — such as `hostRequirements`, port attributes, `waitFor`,
`workspaceMount`, `overrideCommand`, `secrets`, remote-user/UID settings, and
compose overrides — are reported as reserved. Unknown top-level keys are also
reported and not applied; editor-only `customizations` is ignored silently.
This is the supported subset for this thegn version, not a promise to mirror
every current or future containers.dev field.

Supported native OCI behavior includes image, Dockerfile build, compose
service, mounts, forwarded ports, container/process environment, lifecycle
hooks, and in-container feature installs. `overrideFeatureInstallOrder` is
honored; fetched `installsAfter`/`dependsOn` metadata ordering, generated
Dockerfile feature layers, and `devcontainer.metadata` image-label merging are
reserved.

### Lifecycle and provider fallback

After lifecycle approval, `initializeCommand` uses the host-side one-time
prepare hook. `onCreateCommand`, `updateContentCommand`, and
`postCreateCommand` are ordered one-time provisioning steps. In particular,
postCreate is not rerun for every pane. `postStartCommand` and
`postAttachCommand` use the existing per-pane init script and run before each
pane shell — thegn has no distinct attach event, so per-pane is its explicit
per-start/attach analogue.

For a local, fully approved config containing only safely applied fields, an
installed `devcontainer` CLI may be **CLI-ready**. If that bounded provider
probe is unavailable/degraded, the config is not safe to pass through raw, or
`devcontainer up` fails, thegn uses its existing native OCI path for the
supported approved subset. A non-OCI backend cannot honor an
image/build/compose source; doctor reports that degradation instead of
silently claiming the container shape was used.

`thegn doctor` shows the `Devcontainer support` block: mode, repo, candidates,
selected file or selection error, provider, status, trust counts, and backend
honorability. It reads config and approvals and may run bounded
`devcontainer --version`; it never pulls, builds, starts, runs lifecycle code,
or makes a network probe just to report health.

The sidebar and active tab bar use `dc:<selected-path> [<state>]` (or
`dc:[<state>]` before selection). The transient states are `off`, `ambiguous`,
`invalid`, `pending`, `ready`, and `degraded`. `ready` means CLI-ready;
`degraded` can still mean the supported subset is running through the native
OCI fallback. Re-run doctor for the detailed reason.

## Enforcement matrix

`thegn doctor` renders one **enforcement matrix** for this host: per backend,
what it actually enforces —

| Cell      | What it means                                                                 |
| --------- | ----------------------------------------------------------------------------- |
| `fs`      | filesystem isolation: a separate root, unit-level protection, or the host fs  |
| `net`     | network isolation: enforceable, only under `network=none`, or the host stack  |
| `ceiling` | resource cap strength: hard cgroup/VM, soft `nice`, deferred, or none         |
| `scoping` | process-tree scoping: engine lifecycle, pid namespace, unit, job object, pgid |
| `class`   | the honest isolation class (below) — what would have to fail for an escape    |

Every cell is **derived** from the same predicates the resolver uses, so the
matrix can never disagree with what actually launches. The ceiling cell reflects
what is _measured_ on this host — a machine without cgroup cpu delegation shows a
soft ceiling for the host-toolchain backends, not a hard one. Backends thegn's
verbs were never checked against a real install are flagged `(unverified)`.

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

## Isolation floor

`backend_chain` expresses a _preference_; the isolation floor is a _demand_.

```toml
[sandbox]
isolation_floor = "guest-kernel"   # "" | shared-kernel | userspace-kernel | guest-kernel
on_floor_miss   = "fail"           # degrade (default) | fail
```

The floor is compared over the **honest class** of what the launch actually
enters, after backend selection and any `oci_runtime` degrade — so a `krun` that
fell back to the daemon default counts as `shared-kernel`, and a macOS local OCI
container counts as `guest-kernel`. On a miss, `degrade` (the default, fail-safe:
right when you are present to see the warning) launches with the worktree marked
degraded and a warning; `fail` (fail-closed: right for unattended/agent code)
refuses to launch — no process spawns on the host — and names the floor, the best
class available here, and how to satisfy it. A managed provider placement is out
of scope (reported `provider-managed`, never counted as a tier). A repo-root
`.thegn.toml` may only **raise** the floor, never lower it. The two queues'
agent handoff (`[merge_queue]` / `[pr_queue]`) can demand the same floor with
`agent_sandbox` + `agent_isolation_floor`; a fail-closed miss there is an
**infrastructure** failure that holds the entry and never blames the branch.

## Choosing per worktree

The `Alt-w` "what to run" picker offers the sandbox choice when
`[sandbox]` is configured, and `[[agents]]` entries can pin their own. A
repo-root `.thegn.toml` overrides settings for that repo.

## Resource limits and caches

```toml
[sandbox.limits]
cpu    = "2"
memory = "4g"

[sandbox.volumes]          # OCI backends only
cargo-registry = "/root/.cargo/registry"
node-modules   = "/root/.npm"
```

Named volumes survive container recreation, so a package cache does not
cold-start on every build.

## Network

By default a `hardened` sandbox keeps networking so fetch, clone, build,
and debuggers work; `sealed` removes it entirely. `[sandbox]` also
carries a DNS allow/block filter for the middle ground — see
[[config-reference]].

`[sandbox.vpn]` can put the sandbox on an overlay network (tailscale,
headscale, wireguard, openvpn, netbird, zerotier). The `mode` decides how:
`sidecar` (default, best isolation — a companion container owns the
netns), `proxy` (userspace SOCKS/HTTP; **not** a containment boundary, but
the only honest option for bwrap/systemd), `in_container`, or `netns`.
`on_error` decides what happens when the tunnel will not come up: `fail`
refuses to launch, `warn` launches anyway, `offline` forces
`network=none` so nothing leaks onto the host network.

Separately, `[network]` auto-detects when the machine is offline and
pauses remote refreshes rather than hammering unreachable endpoints;
local caches are served stale and it restores on the next success.

## Environments

Named `[env.<name>]` execution environments (local, container, or a cloud
provider) appear in the panel's **system → environments** section — `↵`
binds the cursor env to the active worktree, `t` tests its provider
token (the outcome pops as a toast and is logged), `n` opens the add
wizard, and `x`
removes it after a confirm (removal also forgets the stored token). An
environment can keep a **warm spare pool** so new worktrees start
instantly; raise or lower the active workspace's pool target with the
warm-pool actions (`Ctrl-Alt-=` / `Ctrl-Alt--`, also palette-runnable).
Pool state shows on the sidebar's workspace chip, not in this section.
`[sandbox] default_env` picks the environment new worktrees use.

An environment can keep a **warm spare pool** so new worktrees start
instantly; the warm-pool actions raise and lower the active workspace's
target (palette-runnable and bindable).

## Remote worktrees

`[sandbox.remote] host` runs worktrees on another machine. Control-plane
work (git reads, container lifecycle) always uses ssh; the interactive
pane uses mosh by default, so it survives network changes. `[remote]`
tunes keepalives, timeouts, and retry backoff for flaky links.

> Remote worktrees and cloud providers are **dev-channel only** — see
> [[release-channels]].

## Inspecting

The panel's **system → sandbox** section shows the live state for the
focused worktree: the backend, image, mounts, DNS filtering, and the
container's status and cpu/mem/net when one exists; the wider views add
the activity timeline. `g` widens it from this worktree to every
container on the machine; at full width the containers table sits beside
the timeline. Container actions: `s` stops and `r` restarts the
highlighted container (at the narrow widths, the worktree's own
container), and `l` tails its logs live in a center pane (`<runtime> logs
--tail 200 -f`). Stop and restart run off the UI loop and report their
outcome as a toast.

The machine-global view is the system monitor's **Containers** tab
([[system-monitor]]), which lists every backend's containers with live
stats and lifecycle actions on the thegn-owned ones.

## Cleanup

thegn only ever manages containers it created (the `thegn-` name family and
`thegn.managed`-labelled images/volumes); foreign containers are read-only.
Cleanup is always explicit — thegn never prunes on a schedule.

- `thegn sandbox gc` runs the startup orphan sweep on demand: it removes thegn
  containers whose worktree no longer exists in the registry, across every
  available backend, and reports what it removed. Safe to run any time.
- `thegn sandbox prune` reclaims thegn-owned **stopped** containers and
  `thegn.managed` images and volumes. On a terminal it lists what it would
  remove and asks to confirm; `--yes` skips the prompt for scripts and
  `--dry-run` never removes anything. Narrow it with `--containers`,
  `--images` or `--volumes`. Volumes that carry a **persistent role** (the
  seeded nix-store / cargo warm caches — potential user state) are always kept
  and named in the listing.
- `thegn sandbox prune --host <name>` runs the same owned-only prune on a
  provisioned host over its control channel — the way to reclaim the on-host
  images and volumes that `thegn host rm`/`rm-cache` leave behind.

`thegn doctor` lists, per detected backend, which management operations it
supports.

See [[config-reference]] for every `[sandbox]` key, and [[configuration]]
for how the layers combine.
