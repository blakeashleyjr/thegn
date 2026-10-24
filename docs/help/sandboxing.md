---
id: sandboxing
title: Sandboxing
parent: workflows
order: 3
contexts: [panel:sandbox#inspecting, panel:environments]
actions: [warm-pool-increment, warm-pool-decrement, toolchain-install]
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

| Profile         | What it does                                                                                                                                                                                                |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `open`          | no hardening — the escape hatch                                                                                                                                                                             |
| `standard`      | **default.** read-only root, no-new-privileges, a process cap; network and capabilities intact                                                                                                              |
| `sealed`        | full lockdown: `network=none`, drop ALL caps, tighter pids cap. For untrusted work                                                                                                                          |
| `sealed-tunnel` | `sealed` plus a `[sandbox.vpn]` tunnel as the only route. **Refuses to launch while a VPN is configured**: thegn cannot yet enforce that the tunnel is the only route. With no VPN it behaves like `sealed` |

`hardened` is accepted as an alias for `standard` in existing configuration.

Under `standard`, **everything outside the worktree is read-only —
including your `$HOME`**, so a sandboxed agent cannot `cd` out and modify
host files. That is _integrity_ isolation only: the whole `$HOME` stays
**readable**, credentials included (`~/.ssh`, `~/.aws`, `~/.gnupg`,
`~/.config/gh`, thegn state). `open` leaves `$HOME` readable and writable.

`sealed` / `sealed-tunnel` add _confidentiality_: the host `$HOME` is
**hidden**. The pane sees a private, empty `$HOME` (tmpfs; systemd uses
`ProtectHome=tmpfs`, containers never mount it) with only a reviewed
read-only allowlist of **single files** bound in — `~/.gitconfig`,
`~/.config/git/{config,ignore,attributes,allowed_signers}`, the zsh and
bash rc files (`~/.zshrc`, `~/.config/zsh/.zshrc`, …), `~/.profile`,
`~/.inputrc`, `~/.config/starship.toml` — plus the worktree and build
caches. No directory is bound from the allowlist (`~/.config/git` and
`~/.config/zsh` hold `credentials` and `.zsh_history`). Each entry is
resolved through symlinks and bound from its real target, which must be a
regular file under `$HOME`, `/nix/store`, `/usr` or `/etc`.

The deny list (matched against resolved paths, for `$HOME` and the passwd
home of `$USER` — read from `/etc/passwd` only, so NSS/LDAP users get just
the `$HOME` spelling) covers `~/.ssh`, `~/.aws`, `~/.azure`, `~/.gnupg`,
`~/.secrets`, `~/.netrc`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
`~/.cargo/credentials*`, `~/.vault-token`, `~/.docker`, `~/.kube`,
`~/.config/{gh,gcloud,rclone,git/credentials,…}`, `~/.terraform.d`,
keyrings, browser profiles, `~/.claude*`, `~/.codex`, thegn's state dir,
`/run/agenix`, `/run/secrets`, `/run/user` and `/root`.

**One chokepoint enforces it.** Every launcher (panes, agents, queue agents,
autopilot, cache probes) builds its command through `sandbox::enter_argv` (and
`ensure` for containers; queue agents only when the queue's sandbox opt-in is
on), which — for a sealed spec — drops every mount whose host path or
destination lies inside the deny list, whoever added it: `[sandbox] mounts`, a
bound identity's git ssh key / `GH_CONFIG_DIR` / `GNUPGHOME`, the profile
credential mounts, or an agent provider home. There is no default `~/.gnupg`
grant; add `"~/.gnupg:rw"` to `[sandbox] mounts` yourself if a pane must sign.
Mount entries accept `path`, `path:rw`, `host:dest`, or `host:dest:rw`;
malformed entries are ignored with a warning and listed by
`thegn config validate`. A mount whose host path is, or contains, a protected
path (the worktree being `$HOME` itself, a `~` mount) refuses the launch.
Compose on OCI backends is refused whenever the resolved spec requests
`read_only_root`, `no_new_privileges`, a PID limit, dropped capabilities,
sealed home, network-none or network filters, an OCI runtime, per-pane
CPU/memory limits, or a VPN; Compose cannot enforce those restrictions. This
applies under every profile, including `open`. Compose remains available when
none of those restrictions is requested. Before `up`, thegn inspects the
effective Compose config and refuses a service that binds thegn state, its
runtime/control socket dir, credential dirs under `$HOME` (`.ssh`, `.aws`,
`.gnupg`, `.config/gh`, `.kube`, `.docker`, `.netrc`, `.git-credentials`) or a
container-engine socket. This includes binds of a parent directory,
bind-backed named volumes, and file secrets/configs; there is no per-repo
opt-out. The inspected service model and mount paths are what thegn launches;
Compose files and interpolation inputs are inspected again on the next launch.
If the config cannot be inspected, launch proceeds with a warning and is
reported as host-process isolation. Remote placements are not checked.
After `up`, thegn inspects the running project containers and stops the project
(`compose down`) if any is privileged, uses the host network or PID namespace,
adds `SYS_ADMIN`, `SYS_MODULE`, `SYS_RAWIO`, `SYS_PTRACE`, `DAC_READ_SEARCH` or
`ALL`, or bind-mounts a protected path. If the containers cannot be inspected,
the launch is reported as host-process isolation.
Queue-agent sandboxes get the same protected-bind check, including the daemon
control-socket directory; a protected bind is refused and the queue dispatch is
held. If their Compose config cannot be inspected, the agent runs degraded with
a warning. On backends that store the label, a kept container is reused only if
its `thegn.policy` label matches a fingerprint of the whole resolved policy
and, when sealed, it binds exactly the required mounts; otherwise it is
recreated. On these backends, a policy change recreates it once, and a
container made by an older thegn is recreated once after upgrade. The one
tolerated extra is the devenv `/nix` bind, and only while devenv is still on:
turn devenv off and the container is recreated. Apple containers carry no seal
label, so reuse there is judged on mounts alone, and a sealed Apple container
whose inspect output has no mounts list at all is recreated rather than
trusted. Dropped `[sandbox] mounts` are listed by `thegn doctor`; a
user-authored one also warns once per path. The ssh identity-key mounts and
shell-state XDG write carve-outs are not applied to sealed profiles. Extend the
allowlist with `[sandbox] mounts` (paths outside the deny list only).
A kept container is also reused only while it runs the image id that its
configured image resolves to; a different id or failed image probe recreates
it.

Local Compose services carry the `thegn.policy` label; a missing or stale label
recreates them on the next launch. Pane entry and launch preflight both select
the first service replica, so they enter the same container. A refused sandbox
entry names its refusal class and gives a one-line remediation; for example,
`mount-unresolvable` advises creating the mount source or removing the mount
from the config, then relaunching.

Before launch, every local mount source is resolved to a real path. If a
required source cannot be resolved, the launch is refused with its source and
destination; an unresolvable cache mount is dropped with a warning. A source
that does not exist yet can still resolve through its nearest existing parent.

For local OCI containers that thegn creates directly (not compose), thegn
records each writable, non-cache bind source's identity before creating the
container and checks it again afterward. If the source was swapped, thegn
removes the container and refuses the launch.

**Sealed agent panes need a managed account.** The only exception to the
deny list is the _managed_ per-account credential dir thegn creates under
its own state (`$XDG_STATE_HOME/thegn/accounts/<provider>/<name>`), which
is mounted read-write. The ambient `~/.claude` / `~/.codex` fallback and
adopted account dirs are never mounted into a sealed pane, so without a
managed account the agent runs without host auth.

Managed account homes are created without following symlinks, restricted to
their owner, and stamped with an owner manifest for the provider and account.
Thegn refuses a home that is a symlink, is not a directory, is owned by
another user, or has a malformed or mismatched manifest, and reports why.

If a managed account points to a different home, thegn refuses it and asks you
to log in again; the old directory is left untouched. A configured agent or
tool also refuses to launch if its bundle HOME cannot be prepared. A plain
shell warns and launches without the bundle HOME.

Adding a managed account picks the lowest available `acct<N>` name with no
account row and no existing home directory. Thegn never reuses an existing
home for a new account, so it cannot adopt a forgotten account's login.

The automatic build caches (`~/.cargo/registry`, sccache, nix, `~/.m2`,
gradle) stay writable: that is an _integrity_ exposure (a sealed pane could
poison a shared cache), not a confidentiality one. The allowlist and every
mount are re-validated when the command line is built; a swap in the
instant between that check and the runtime starting remains possible.

A sealed launch that cannot hide `$HOME` is **refused**, never run with a
weaker `sealed` and never downgraded to a host shell: a backend with no
filesystem boundary (`none`/host, Windows, WSL), `file_access = "all"`, no
backend producing a sandbox at all, an unset or unresolvable `$HOME`, a
systemd launch whose `$HOME` is outside `/home`, `/root` and `/run/user`
(where `ProtectHome=tmpfs` does not reach), a systemd or container mount
path containing whitespace, a quote, a backslash or `:`, or a terminal
anchored at `$HOME` itself. Setting `[sandbox] enabled = false` is your
explicit choice and is not overridden: the profile is then simply not
enforced, and `thegn doctor` says so. `thegn doctor` prints
`home  hidden | read-only | writable` for the configured profile.

A sealed profile is strict: if a backend cannot enforce every sealed control,
the launch is refused. With `backend = "auto"`, thegn tries the next backend;
an explicitly selected backend is refused with the unsupported controls.

Writable carve-outs (non-sealed): the worktree and its git dir, build
caches, `/tmp`, and a narrow set of shell-state paths (`~/tmp`,
`$XDG_STATE_HOME/{bash,zsh}`, `$XDG_DATA_HOME/{fish,zoxide,atuin}`,
`~/.zsh_history`, `~/.bash_history`). The XDG roots default to
`~/.local/state` and `~/.local/share`; configured absolute roots outside
`$HOME` work too. Only those application leaves are mounted. Missing,
relative, symlinked, non-UTF-8, or overlapping roots are omitted. Agent config
dirs (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`, or the `~/.claude` / `~/.codex` defaults) are writable for
every non-sealed pane, so an agent CLI you run by hand can persist its
state instead of dying on a read-only filesystem.

> `~/.keychain` is carved writable for `standard` panes but **not** for
> `sealed` ones: it holds scripts your host login shells later _source_,
> so a writable copy from a sealed sandbox would be a persistence vector.

The daemon socket normally lives under `$XDG_RUNTIME_DIR/thegn`. With that
variable unset, it uses a vetted private runtime directory, then a private
`/tmp/thegn-<uid>-<state-hash>/` directory if needed. If neither can be secured,
panes run in-process. It does not place a new control socket under sandbox state.
If that predictable `/tmp` directory belongs to another user or has the wrong
permissions, the daemon uses a random owner-only sibling and records its name in
an owner-only locator under the state directory. A locator that does not name a
valid owner-only directory is ignored, and the daemon replaces it on its next
start.
At startup, the daemon removes same-state random siblings the locator no longer
names only when they are owner-only directories containing no entries beyond a
socket that refuses connections (or is absent) and an unlocked, owner-owned
lock file (if present). Unproven siblings are left in place.
If a control socket directory is visible through a sandbox bind (for example,
explicit `[daemon] socket` or `XDG_RUNTIME_DIR` under `$HOME` or the worktree),
bwrap hides it with a tmpfs. systemd makes it inaccessible unless that would hide
the worktree or a carved-back mount, in which case it refuses when
`read_only_root = true`. OCI refuses any mount exposing it. The relocated
private `/tmp` directory is guarded with the predictable fallback. State `run/`
(locator and lock) is write-protected: bwrap hides it when present and refuses
if it is absent under a writable bind; systemd makes it inaccessible only when
`read_only_root = true`; OCI refuses writable mounts covering it while admitting
read-only `$HOME`. With `[daemon] enabled = false`, the compositor prepares
`run/` before launching panes, so writable-root profiles can mask it. The
hardcoded writable parents are private tmpfs mounts at `/tmp` and the sealed
`$HOME`; neither exposes host state. The `--dev-bind / /` used by `all` and
`host` is excluded by the guard. Those access modes expose the host by design.

Standard, hardened, and sealed panes do not inherit `DBUS_SESSION_BUS_ADDRESS`.
Bwrap unsets it, and systemd panes also cannot reach `$XDG_RUNTIME_DIR/bus` or
`$XDG_RUNTIME_DIR/systemd`; `profile = "open"` keeps the address. An
abstract-namespace session bus can still be reached from bwrap panes that share
the host network, because the address is withheld but the network namespace is shared.

## Backends

`[sandbox] backend = "auto"` walks the chain until one works:
`podman-rootless` → `podman-rootful` → `docker` → `apple` → `bwrap` →
`jobobject` (Windows). There is no host fallback: with no usable backend, a
sandboxed launch is refused under the default floor. Pin one to skip the
probing — `podman-rootless`, `podman-rootful`, `docker`, `bwrap`, `systemd`,
`apple`, `wsl`, `none`.

The outer `bwrap` and `systemd-run` launchers are resolved to absolute paths
from thegn's own `PATH`. If either is missing there, launch is refused rather
than resolving it from the pane environment. Loader variables such as `LD_*`
and `DYLD_*` are withheld from the outer launcher. Under `bwrap` they are set
again inside the sandbox; a `systemd-run` launch without `bwrap` drops them.

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

> **Platform status:** the public alpha supports Linux on x86_64, where
> `bwrap`, `systemd`, podman and docker are the backends to use. The `apple`
> backend and macOS are best-effort; the Windows `jobobject` and `wsl` entries
> are not supported in this alpha.

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

Under a named profile (`--profile <name>`), ambient credentials are denied
by default for sandboxes, devcontainer `${localEnv:}` substitutions and
provider placements. Only infrastructure names such as `PATH`, `HOME`,
`TERM` and `LANG`, `LC_*`, `THEGN_*`, and names in
`[profiles.<name>.sandbox] env_passthrough` are shared; that profile list
replaces `[sandbox] env_passthrough`. At startup, the thegn process also unsets
known credential environment families, keeping `SSH_AUTH_SOCK` only when the
profile has `ssh/id`. Its git, gh and GnuPG directories are mounted read-only in
sandboxes; a pane that needs to write them, such as `gh auth login` or GnuPG
signing, needs an explicit `[sandbox] mounts` grant. Startup refuses if the
profile state or credential directories cannot be created. Startup also
refuses if `ssh/id` exists but its target is not a regular file owned by you
with no group or other access.
The default profile is unchanged.
Named profile directories are created owner-only without following symlinks.
`thegn doctor` lists denied and shared names, never values. The share list is
read at startup.

Some repo requests are never eligible for approval. `privileged`, `capAdd`,
`securityOpt`, `runArgs`, and `init` could weaken the trusted sandbox, so thegn
always refuses them and names the reason. Recognized fields that thegn does not
apply — such as `hostRequirements`, port attributes, `waitFor`,
`workspaceMount`, `overrideCommand`, `secrets`, remote-user/UID settings, and
compose overrides — are reported as reserved. Unknown top-level keys are also
reported and not applied; editor-only `customizations` is ignored silently.
This is the supported subset for this thegn version, not a promise to mirror
every current or future containers.dev field.

Named volumes declared by a devcontainer carry an owner label. A volume labelled
for another devcontainer is refused; a legacy unlabelled volume is claimed on
first local use, while an unlabelled volume on a remote host is refused.
Volumes configured under `[sandbox] volumes` are not owner-bound.

Supported native OCI behavior includes image, Dockerfile build, compose
service, mounts, forwarded ports, container/process environment, lifecycle
hooks, and in-container feature installs. `overrideFeatureInstallOrder` is
honored; fetched `installsAfter`/`dependsOn` metadata ordering, generated
Dockerfile feature layers, and `devcontainer.metadata` image-label merging are
reserved.

The image thegn builds for a devcontainer `build` is tagged from the build
declaration and Dockerfile contents, so changing the Dockerfile rebuilds it on
the next launch. If the Dockerfile cannot be read for hashing, thegn warns and
uses the declaration alone for the tag.

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
silently claiming the container shape was used. While falling back between
backends, a backend whose launch probe failed is remembered for the same
environment, placement, and worktree; later launches skip it with a warning that
the earlier probe failed, while current policy and isolation checks still run.
The memory is dropped when the config is reloaded, and for a worktree when you
retry, start its runtime, or run it on the host. An explicitly chosen backend is
always probed.

The `devcontainer` CLI provider's trust digest covers the selected
`devcontainer.json` and the bytes of its referenced Dockerfile and Compose
files inside the repository. The digest is checked before `devcontainer up`
and each exec; changing one of those files requires fresh approval. A missing,
absolute, or repository-escaping reference refuses provider use.

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
| `net`     | `netns; rules DNS-only`, `only when network=none`, or `none (host net)`       |
| `ceiling` | resource cap strength: hard cgroup/VM, soft `nice`, deferred, or none         |
| `scoping` | process-tree scoping: engine lifecycle, pid namespace, unit, job object, pgid |
| `class`   | the honest isolation class (below) — what would have to fail for an escape    |

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
reset. `XDG_RUNTIME_DIR` and `GIT_CONFIG_GLOBAL` are kept for launcher plumbing
and commit identity. A broad `[sandbox] mounts` entry that exposes the whole
home still exposes these directories; they are not masked inside a broad home
mount. With `agent_sandbox` on, a queue agent whose sandbox cannot be
established is held; it never falls back to the host. Interactive panes are
unchanged.

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

`[sandbox] compiler_cache = "off"` is the default and is the only switch that
authorizes an sccache wrapper inside containment. It removes inherited sccache
wrappers and endpoint variables even when `[disk].sccache` or the Nix dev shell
sets them; unrelated custom `RUSTC_WRAPPER` values are preserved. Set it to
`"auto"` to let a local bwrap sandbox probe sccache, mount only its data
directory, and use a sandbox-private Unix socket. A failed probe or later
server/transport I/O error degrades visibly to plain rustc instead of failing
the Cargo build; a real compiler error is preserved and is never retried or
reported as cache degradation. `thegn doctor` shows the host inputs and the last contained
reachability/write probe separately. Backends without a proven transport use
plain rustc.

## Network

By default a `standard` sandbox keeps networking so fetch, clone, build,
and debuggers work; `sealed` removes it entirely. `[sandbox]` also
carries DNS-name allow/block rules for the middle ground. These rules are not
an egress boundary; `network = "none"` is the only enforced deny, and `*` in
`network_block` means offline — see [[config-reference]].
Standalone sandboxed terminals also warn when DNS-name rules apply, because
those rules are not an egress boundary.

`[sandbox.vpn]` can put the sandbox on an overlay network (tailscale,
headscale, wireguard, openvpn, netbird, zerotier). The `mode` decides how:
`sidecar` (default, best isolation — a companion container owns the
netns), `proxy` (userspace SOCKS/HTTP; **not** a containment boundary, but
the only honest option for bwrap/systemd), `in_container`, or `netns`.
`on_error` decides what happens when the tunnel will not come up: `fail`
refuses to launch, `warn` launches anyway, `offline` forces
`network=none` so nothing leaks onto the host network.

A running VPN sidecar is reused only when its tunnel configuration matches.
Otherwise the result follows `on_error`: `fail` refuses launch, `warn`
continues without the VPN, and `offline` forces `network=none`. The sidecar
stays running. Stop this worktree's sandbox (close the worktree, or remove both
the worktree container and sidecar) so both are recreated with the new
configuration. An unlabelled sidecar from before this check counts as changed.

Separately, `[network]` auto-detects when the machine is offline and
pauses remote refreshes rather than hammering unreachable endpoints;
local caches are served stale and it restores on the next success.

## Environments

Named `[env.<name>]` execution environments (local, container, or a cloud
provider) appear in the panel's **system → environments** section — `↵`
(re)binds the cursor env to the active worktree, `t` tests its provider
token (the outcome pops as a toast and is logged), `n` opens the add
wizard, and `x`
removes it after a confirm (removal also forgets the stored token). An
environment can keep a **warm spare pool** so new worktrees start
instantly; raise or lower the active project's pool target with the
warm-pool actions (`Ctrl-Alt-=` / `Ctrl-Alt--`, also palette-runnable). The
target update runs in the background: `warm pool: updating target…` appears
while it is queued, followed by the result, and raising the target stops at
eight spares for the workspace's environment.
If the spare ledger cannot be read, reconciliation warns and makes no pool
changes rather than treating the pool as empty.
If the current warm-pool target cannot be read, reconciliation makes no pool
changes rather than falling back to the configured size. The warm-pool keys
report `warm pool: cannot read the current target: ...` and save nothing.
Reconciliation halts, such as lease, ledger, target, or spare-read failures,
appear as a status-line toast; the same halt is shown once until it clears or
its message changes. When deleting a worktree cannot recycle a claimed spare,
its ledger row remains until the provider confirms destruction, so a failed
destroy leaves it listed for a later retry.
A stale or released spare is recycled from its base checkpoint only when the
provider's checkpoint list confirms that checkpoint exists; if the list cannot
be read, the spare is destroyed instead.

If a worktree's environment selection cannot be read from the state database,
thegn never falls back to the default environment. `thegn env` verbs report an
error; worktree deletion refuses before removing anything; closing a tab does
path-only cleanup and warns that an SSH sandbox container may remain. On the
dev channel, provider SSH connect, warm-pool operations, hibernation,
provisioning and host placement are refused or skipped with the reason. The
close auto-checkpoint is also skipped with a warning on the dev channel.

When the state database exists but the worktree's recorded location cannot be
read or is unrecognised, writes and code runs refuse instead of using the host
checkout. This covers panel git actions, Tests and Jobs, `thegn land`, mutating
`thegn pr` actions, `thegn ci rerun`, `thegn ci trigger` and `thegn ci cancel`,
worktree delete, Sandbox container stop or restart, clipboard image paste and
toolchain install. With no state database, worktrees remain local as before.

If an environment selection cannot be read and a non-local `[env.*]` is
configured (dev channel), the tab shows `env?` and the loading splash shows
`env: unknown`. `thegn doctor` reports an unknown environment; a failed pane
connection is not blamed on a guessed provider. Provisioning skips trust-gated
devcontainer steps and launch skips devshell injection, with a warning. On the
dev channel, an unreadable account selection refuses a provider pane; failed
SSH sandbox teardown identifies the container that may still be running, and
hibernation refuses a restore when its record cannot be read while hibernation
is enabled. Teardown and close auto-checkpoint warnings appear on the status
line and in a toast while the TUI runs; the CLI prints them to stderr.

Pool state shows on the sidebar's project chip, not in this section.
`thegn doctor` lists claimed warm-pool spares without a bound worktree and
quarantined spares under `Warm pool (provider spares needing review)`; it will
not destroy them on its own. `thegn doctor --json` includes the same report in
`warm_pool`.
`[sandbox] default_env` picks the environment new worktrees use.
A repo's `env =` setting is only a hint; if that environment would weaken
user policy, it is ignored with a warning.

### Worktree toolchains

When a worktree declares a mise or asdf toolchain, thegn adds its safe shims to
the pane `PATH`. The optional mise environment layer is lower precedence than
an environment bundle or devshell and is applied only after the current repo
config set is approved. Use the command-palette action `toolchain-install` to
run the bounded, explicit install operation; opening a pane never downloads
tools. A `(mise ~)` token means approval is pending, while `(mise !)` means
the provider or shims are unavailable. Remote worktrees remain responsible
for installing their tools on the remote target.

Cached flake devShell injection (`[sandbox] inject_devshell`) is off by
default. Turn it on in your own config (globally, per project or per
environment), or approve a repository's `.thegn.toml` request with
`thegn repo trust . --approve <id>`; an unapproved request is listed as pending
and changes nothing.

## Lifecycle hooks and pane setup

`[hooks]` commands are host-side worktree/session lifecycle jobs. They run
outside the sandbox with a curated environment, capture stdout/stderr in the
worktree state log, and use the event-specific repository/worktree directory;
see [[configuration]] for the six events, ordering, trust gate, timeout, and
failure policy. The legacy `[sandbox].prepare` list is the compatibility
alias for `post_create`, not a second fire-and-forget path.

`[sandbox].init_script` remains separate: it is a POSIX shell snippet executed
inside the sandbox before each agent or shell pane starts. Use it for
per-pane/container setup; use lifecycle hooks for host-side worktree setup,
teardown, or session edges.

## Remote worktrees

`[sandbox.remote] host` runs worktrees on another machine. Control-plane
work (git reads, container lifecycle) always uses ssh; the interactive
pane uses mosh by default, so it survives network changes. `[remote]`
tunes keepalives, timeouts, and retry backoff for flaky links.

> Remote worktrees and cloud providers are **dev-channel only** — see
> [[release-channels]].

The bridge helper is uploaded to a staged file and checked by SHA-256 before
it is moved into place. If the install cannot be verified, the bridge
connection is skipped.
The helper lives at `/var/tmp/thegn-bridge/<sha256>/thegn`, outside the
worktree. Its digest is re-checked before thegn starts it through a provider's
native exec API or for a reverse tunnel.
A provider launch through its control command, such as a vendor CLI or SSH,
checks the digest again before running the helper; changed bytes are refused
with `thegn: resident bridge digest mismatch`.
When a provider's `exec_command` uses `ssh` with a host, its checked bridge
launch is passed to the remote shell as one quoted word.

A remote checkout with commits or changes made since thegn last synced it is kept for recovery, and thegn shows how to recover it.

On provider placements, an explicit isolation request thegn cannot enforce is
refused before provisioning, credential access or upload. Without such a
request, the placement runs as `provider-managed (unverified)`; `thegn doctor`
shows that floor.

A thegn-derived VPS or Fly sandbox name is bound to the worktree that created
it. Another worktree resolving to the same name is refused before a provider
call.
VPS and Fly ledger records carry a credential-free fingerprint of the provider
kind, API authority, and configured (or default) token variable name, never the
token itself. A name recorded under another account is refused; the VPS reaper
quarantines that record and does not delete it.
The Fly reaper also quarantines records for another account without revoking or
destroying them. If it cannot determine the current account, it quarantines
every record before making provider calls.
If the UI is running, a change to the non-empty set of resources a VPS or
Fly reaper keeps for review raises one alert toast for that provider, naming
the first and counting the rest. An unchanged set does not repeat it.
A VPS or Fly `max_instances` cap
counts sandboxes for the same provider account; records without an account or
that cannot be read count against every account.
On the dev channel, `max_lifetime_secs` is enforced by the Fly and VPS reapers
(Hetzner and DigitalOcean). Providers without a lifetime reaper, such as
machine0 and Sprites, do not enforce it; `thegn config validate` warns when it
is set. Destroy those instances yourself or set the value to `0`.

On the dev channel, an unreadable or malformed VPS or Fly ledger record is
refused rather than treated as absent. Destroy, retarget, address lookup,
`vps-ssh` and managed-key selection refuse it; a refused destroy keeps SSH
custody and tells you to fix or remove the record before retrying. SSH keys
registered with Hetzner, DigitalOcean and machine0 use `thegn-managed-`
followed by the first 32 hexadecimal characters of the SHA-256 digest of the
decoded public key. An invalid OpenSSH public key is refused.

On the dev channel, the Fly backend strictly parses an app's machine list and
rejects malformed lists or duplicate machine IDs. If more than one live
thegn-managed machine is present, it refuses to choose one.

Machine0 creates and restores are serialized across thegn processes by an
owner-only lock in the VPS registry. A create that cannot get the lock within
the six-minute readiness budget is refused with a busy message.
Machine0 image, SSH-key and VM lists are parsed strictly: an unrecognised
response shape, an unidentifiable row or a duplicate entry fails the operation
instead of being treated as an empty or shorter list. Machine0 readiness,
status and SSH-auth waits, and Fly's sshd probe, count each in-flight request
against their total time budget.
Machine0 records a pending reservation before creating a VM, and reservations
count toward `max_instances`. If the create outcome is unknown, such as after a
lost response, the reservation remains; retrying the create, or reopening the
worktree once machine0 lists the VM, adopts it (waits for it, applies
provisioning, and records its SSH key) instead of creating a duplicate. The
reservation is retired only after adoption is ready. The error says to retry
later or destroy the sandbox to retire the reservation. A VM with that name
and no pending create from this machine is refused. The Sprites list follows
every page and refuses results over its page or row limit, or an ambiguous
continuation, rather than treating a partial list as complete. Configured
provider ids at `env.<name>.provider.id` must be lowercase for Sprites,
Hetzner, DigitalOcean, Fly and machine0; validation refuses uppercase ids.

VPS and machine0 sandboxes pin their SSH host key on first contact with
`accept-new` in a per-instance file under `~/.thegn/vps/known_hosts.d/` (or
`$THEGN_DIR/vps/known_hosts.d/`). Thegn creates that directory with owner-only
permissions before managed SSH connections, refuses a symlinked or
non-private directory, requires the same key on later connections, and
removes the pin when it destroys a VPS or machine0 sandbox. Creating a machine0
VM also clears a stale pin left by an earlier VM with the same name.

If local parity fails during provider provisioning, autonomous launches on
that worktree are refused until a later provision applies parity cleanly;
interactive panes can still open. Host-parity closure pushes use a
per-installation signing key and are skipped with a warning rather than sent
unsigned when signing is unavailable. The iroh call-home reach is available
only in builds with the `iroh` feature; `thegn doctor` reports whether it is
built in.

For remote and provider worktrees, thegn reads `.gitmodules` from the remote
commit and rechecks approved submodules before initializing them. It validates
the remote worktree sync plan before uploading anything. A provider sandbox's
network policy is applied and read back before its first command; a mismatch
stops provisioning. If an existing sandbox's policy cannot be verified, thegn
refuses to provision it and leaves it in place.
For provider sandboxes, network allow and block entries are trimmed,
lowercased, stripped of a trailing dot, and deduplicated before the provider
policy request; non-ASCII entries (use `xn--` punycode), IP literals, and
malformed domain patterns are refused with the entry named in the error. Any
allow entry keeps the final deny-everything-else rule, even when blocks shadow
all allow entries.
A provider launch using `sealed-tunnel` with a VPN configured is refused before
provisioning; if a refused launch created its provider sandbox, thegn destroys
that sandbox.

With `data = "sync"`, closing the worktree copies sandbox changes back by
comparing each file with what the upload sent: a file the sandbox did not
change never overwrites a local edit. A file changed in both places (or deleted
locally and changed in the sandbox) stops the sync with a list of files; nothing
is written locally and the sandbox is kept. Make each listed file match one
copy and close again. Files and directories your `.gitignore` or `.ignore` files
exclude are not compared or copied back (the upload skips them too), so build
output never blocks the sync; a file excluded only by a global or
`.git/info/exclude` rule is still compared. Files deleted in the sandbox are not
deleted locally.

## Inspecting

The panel's **system → sandbox** section shows the live state for the
focused worktree: the backend, image, mounts, DNS filtering, and the
container's status and cpu/mem/net when one exists; the wider views add
the activity timeline. Containers not proven to belong to this profile are
listed read-only: ownership requires this profile's expected container name
(or its recognized `-tgvpn`/`-tgagent` companion) for a known worktree, on
that worktree's launch backend. A user's `thegn-foo`, another profile's
container, or the same name in the other Podman store is foreign; a new
container may appear foreign until the next refresh. `s` and `r` are refused
with a status for foreign containers, while `l` opens a live logs pane
(`<runtime> logs --tail 200 -f`). Actions on owned containers ask first.
`g` widens to every container; it scopes the full table too, and Half-width
`g` rows are selectable. When sandboxing is off, a one-line banner says so.
Stop and restart work runs off the UI loop and reports its outcome as a toast.

The machine-global view is the system monitor's **Containers** tab
([[system-monitor]]), which lists every backend's containers with live
stats and lifecycle actions on the thegn-owned ones.

## Cleanup

Panel and monitor actions apply only to containers proven to belong to this
profile for a known worktree on its launch backend; cleanup recognizes the
`thegn-` name family and `thegn.managed`-labelled images and volumes.
Cleanup is always explicit — thegn never prunes on a schedule.

- `thegn sandbox gc` runs the startup orphan sweep on demand: it removes thegn
  containers with this install/profile's `thegn.owner` label that are provably
  orphaned across available backends. Local compose services carry the owner
  label through a generated override; each is recreated once on its next `up`,
  so gc can reclaim services from a dead worktree. Remote and profile-gated
  compose services stay unlabelled and are kept as unverified. Unlabelled
  containers and containers owned by another profile are listed but never
  removed. Compose project names include a short hash of the worktree path, so
  similarly slugged paths do not share a project. An older project name is
  adopted only when its Compose working directory is inside the worktree; a
  legacy project with a working directory outside it is never adopted and
  starts under the new name. It also removes VPN secret staging directories
  for sidecars it removes, plus staging directories older than an hour whose
  sidecar is gone when at least one runtime could be listed; symlinks are
  never followed.
  Containers thegn creates carry `thegn.worktree` and `thegn.role` labels.
  The sweep keeps a labelled container for a known worktree, removes one only
  when its worktree is gone, and keeps one whose labels it cannot interpret.
  Containers without those labels are judged as before.
  At startup, the sweep runs only when the effective configuration can produce
  a sandbox container (enabled globally or by the active profile, an env
  overlay, or a repo overlay for a known worktree); `thegn sandbox gc` runs it
  regardless.
  It also removes Compose project adoption records older than an hour whose
  worktree is gone, both during gc and the startup sweep; an empty worktree list
  leaves those records alone.
  Safe to run any time.
  When the startup sweep removes containers, the status
  line reports `sandbox: removed <n> orphan container(s): <names>`.
- `thegn sandbox prune` reclaims **thegn-owned (`thegn-` name family) stopped
  containers**, and `thegn.managed` images and volumes. On a terminal it
  lists what it would remove and asks to confirm; `--yes` skips the prompt for
  scripts and `--dry-run` never removes anything. Narrow it with `--containers`,
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
