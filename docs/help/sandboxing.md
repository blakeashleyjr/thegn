---
id: sandboxing
title: Sandboxing
parent: workflows
order: 3
contexts: [panel:sandbox#inspecting]
---

# Sandboxing

Each worktree's interactive process — often a coding agent — can run in a
container while the worktree itself **stays on the host**, bind-mounted at
its real path. That is the whole trick: host-side git keeps working, so
the [[panel]] diff and the [[sidebar]] state are live and unsandboxed
while the process is contained.

## Topics

- [[sandbox-backends]] — backends, the enforcement matrix, the isolation floor and the git write policy
- [[sandbox-devcontainers]] — running a worktree in the repository's devcontainer
- [[sandbox-resources]] — CPU, memory and process limits, and caches
- [[sandbox-network]] — network modes and the allow and block lists
- [[environments]] — named environments, worktree toolchains, lifecycle hooks, remote worktrees and how a remote pane behaves (terminal identity, close versus quit, background setup)
- [[sandbox-profiles]] — pinning a profile per worktree
- [[sandbox-security-model]] — what the sandbox protects, what it does not, and which launches run on the host

## Hardening profiles

`standard` (the default) is an **accident guard**: it is **not a boundary against
a hostile agent** (home readable, network direct by default, shared kernel).
`sealed` is the hardened tier. See [[sandbox-security-model]].

`[sandbox] profile` picks a bundle of isolation knobs. Set it globally,
per repo via a `.thegn.toml` overlay, or per run with
`THEGN_SANDBOX_PROFILE`. A single worktree can also pin its own profile; see
[[sandbox-profiles]].

| Profile         | What it does                                                                                                                                                                                                |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `open`          | no hardening — the escape hatch                                                                                                                                                                             |
| `standard`      | **default.** read-only root, no-new-privileges, a process cap on podman, docker and systemd only; network and capabilities intact                                                                           |
| `sealed`        | full lockdown: `network=none`, drop ALL caps, tighter pids cap. For untrusted work                                                                                                                          |
| `sealed-tunnel` | `sealed` plus a `[sandbox.vpn]` tunnel as the only route. **Refuses to launch while a VPN is configured**: thegn cannot yet enforce that the tunnel is the only route. With no VPN it behaves like `sealed` |

`hardened` is accepted as an alias for `standard` in existing configuration.

When a session prompt names an absolute path under `/tmp`, thegn warns before
launching if the effective bwrap or read-only container sandbox gives that
worker a private `/tmp`. The warning does not block the launch; write reports
and other artifacts under the worktree instead, for example `.thegn/`, because
files written to the sandbox's `/tmp` disappear when the session ends.

### Keeping a report directory

For a local bwrap/OCI/systemd sandbox, bind a writable host directory into
`/tmp` so reports survive the session:

```toml
[env.pipeline.sandbox]
mounts = [
  "/tmp/reports",
  "~/.gitconfig:ro",
]
```

Create the host directory first with `mkdir -p /tmp/reports`. A mount whose
host source does not exist is skipped silently. The env-level `mounts` list
replaces the base `[sandbox] mounts` list rather than extending it, so the
example repeats the default `~/.gitconfig:ro` entry; use
`[workspace.<slug>] sandbox_mounts` when you want an additive workspace mount.
A `:ro` mount does not silence the private-`/tmp` warning because it cannot
receive newly written reports. This host-directory behavior applies to local
bwrap/OCI/systemd placements; remote and provider placements do not honor host
mounts the same way. Sealed
profiles still apply their credential deny list, although `/tmp` report paths
are outside it.

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
sealed home, host or disabled networking, network filters, egress policy, an
OCI runtime, per-pane CPU/memory limits, or a VPN; Compose cannot enforce
those restrictions. This
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
from the config, then relaunching. Compose preflight distinguishes a missing
service, one with no running container, and a scaled service whose replica 1 is
not running. For a scaled service whose replica 1 is not running, bring it back,
for example with `docker compose up -d <service>`.

When sandbox preparation fails with a typed refusal, its message leads with
the refusal class and remediation, for example `sandbox bwrap failed: sandbox
entry refused [<class>]: <remediation>`. This detail is used in launch
warnings, the new-worktree wizard and while preparing a worktree's pipeline.
Failures without a typed class keep their plain text.

Before launch, every local mount source is resolved to a real path. If a
required source cannot be resolved, the launch is refused with its source and
destination; an unresolvable cache mount is dropped with a warning. A source
that does not exist yet can still resolve through its nearest existing parent.

For local OCI containers that thegn creates directly (not compose), thegn
records each writable, non-cache bind source's identity before creating the
container and checks it again afterward. If the source was swapped, thegn
removes the container and refuses the launch.
If a container with this worktree's container name belongs to another worktree
(paths differing only in punctuation or case can share a name), or its owner
cannot be read, the launch is refused with the `container-owner` class and the
other container stays running.
Close that worktree's sandbox first or rename one worktree directory. An older
container without an owner label follows the existing reuse and recreate checks.

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
state instead of dying on a read-only filesystem. A carve-out whose path
has a symlink in any component below `$HOME` is skipped with a warning, never
followed; the pane launches without it. `$HOME` itself may be reached through
a system link (`/home -> /var/home`).

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
When the daemon allocates a private fallback directory, it removes same-state random siblings the locator no longer
names only when they are owner-only directories containing no entries beyond a
socket that refuses connections (or is absent) and an unlocked, owner-owned
lock file (if present). It also removes owner-only directories of other state
keys of the same user under the same proof once nothing in them has changed for
24 hours (at most 64 are examined per allocation). Unproven siblings are left in
place.
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

## Choosing per worktree

The `Alt-w` "what to run" picker offers the sandbox choice when
`[sandbox]` is configured, and `[[agents]]` entries can pin their own. A
repo-root `.thegn.toml` overrides settings for that repo.

## The Lead's sandbox

The Lead (`[lead]`, experimental) runs its agent under bubblewrap. Its
`[sandbox]` settings do not apply. It always uses this fixed layout:

- **Read-only:** the whole host filesystem, including every repo and worktree.
  Writing inside a repo fails with `Read-only file system`.
- **Writable:** the notes dir (`[lead] notes_dir`, default
  `$XDG_STATE_HOME/thegn/lead`) and a private agent home,
  `$XDG_STATE_HOME/thegn/lead-harness/<agent>` (`CLAUDE_CONFIG_DIR` etc. point
  there). The Lead never writes your `~/.claude`; log in once inside the Lead.
  The Lead starts in the notes dir.
- **Hidden:** `$XDG_RUNTIME_DIR`, the daemon socket dir, the state `run` dir,
  every control-socket location and `/tmp` are replaced with empty dirs. The
  rootful docker and podman sockets and the system D-Bus socket are hidden
  too. `THEGN_CONTROL_SOCKET`,
  `DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`, `DISPLAY`, `WAYLAND_DISPLAY`
  and `XAUTHORITY` are unset.
  `~/.ssh`, `~/.gnupg`, `~/.config/gh`, `~/.config/glab-cli`,
  `~/.git-credentials` and `~/.netrc` are hidden too.
- **thegn's state is hidden:** `$XDG_STATE_HOME/thegn`, `$XDG_DATA_HOME/thegn`,
  `$XDG_CACHE_HOME/thegn`, thegn's home (`~/.thegn` or `$THEGN_DIR`) and the
  legacy pre-rename dirs are replaced with empty dirs. That covers the
  database, logs, scrollback, gate target dirs, provider ssh keys and other
  workers' transcripts. Registered repos beneath them come back read-only.
  The agent homes `~/.claude`, `~/.codex` and `~/.pi` are hidden the same way,
  and `~/.claude.json` reads as empty. Only three things come back inside
  them: the notes dir and the private agent home (writable), and the Lead's
  own MCP config and token files (read-only). `$XDG_CONFIG_HOME/thegn` is
  hidden too, except `config.toml`, which the `thegn mcp` commands inside the
  sandbox load. Only `config.toml` itself is readable; files named by `file:`
  secret references are hidden. A literal secret pasted into config.toml is
  readable by the Lead.
- **Reading state:** the Lead sees thegn's state only through its tools:
  `overview.get`, `notifications.list`, `sessions.snapshot`,
  `automations.runs` and `approvals.list`.
- **Network (`[lead] network = "isolated"`, the default):** the Lead runs in
  its own network namespace through `pasta` (from the passt package). It has
  outbound internet, so the model API and remote MCP servers work. Host
  loopback services are out of reach, and so are abstract unix sockets,
  including the X11 display socket, because they belong to the host's
  namespace. Only the Lead listener port is forwarded, at the same
  `127.0.0.1:<port>`, so the Lead still reaches the daemon with its own scoped
  token. A generated `resolv.conf` replaces `/etc/resolv.conf` inside the
  sandbox and names pasta's DNS forwarder (`169.254.1.1`), which relays to
  your host resolver. Your host's LAN address stays reachable.
- **Network (`[lead] network = "host"`):** the Lead shares the host's network.
  It can reach every loopback service on your machine, and every abstract
  unix socket, including an X11 display. Any of those that trusts a local
  caller trusts the Lead. thegn logs a warning on every launch in this mode.
  Use it only when pasta cannot be installed.

The daemon's unix socket, which grants admin to any process of your user, is
out of reach in both modes.

thegn refuses to open the Lead when:

- bwrap is missing. The error is "the Lead needs bwrap (Linux); not available
  here". It never falls back to a host shell.
- The network is `isolated` and pasta is missing. The error is "the Lead's
  isolated network needs pasta (passt); install it or set [lead] network =
  "host"". It never falls back to the host network on its own.
- `[lead] notes_dir` or an agent state dir is inside a registered repo or
  worktree, contains one, or is `/` or `$HOME`. Symlinks are resolved first.
- A writable path overlaps a hidden control dir, or lies inside hidden
  state without being the notes dir or the private agent home.

`[lead] account` is refused for now.

### What the Lead may ask the daemon to do

The sandbox alone is not the boundary: the daemon runs outside it. So the
daemon treats every call made with a Lead token, or arriving on the Lead's
listener, as Lead-confined, whatever its scopes. A Lead-confined call:

- **May open a session** only by naming a configured `[[agents]]` entry (or a
  pipeline stage of one) in a registered worktree. The daemon resolves the
  launch the usual way and refuses it unless the worker really runs in a
  sandbox (bwrap, podman, docker, smolmachines or apple). A worktree that
  resolves to `host` (or `systemd`, which only caps resources) is refused with
  a message naming the worktree and its backend. A raw `argv`, an `env`
  override, a `cwd` outside the worktree, `widen_permissions`, a fork and
  prompts over 64 KiB are refused.
- **May type into a worker it opened** (`sessions.input`), to answer a worker
  that is waiting on a question. The daemon records which sessions a
  Lead-confined `sessions.open` launched and allows input only into a live one
  of those; your own sessions, a finished session and an unknown id are
  refused. Once you type into the worker yourself, the Lead can no longer type
  into it. The input must be plain text plus Enter, at most 16 KiB: escape
  sequences (ESC, CSI, OSC, bracketed-paste markers) and control characters
  other than newline, carriage return and tab are refused. The record is kept
  in the daemon's memory, so after a daemon restart the Lead cannot type into
  anything until it opens a new worker. The MCP tool still needs
  `thegn mcp serve --allow-session-input`.
- **May not** attach to, resize, split or fork a session, run host git (`git.stage`, `git.commit`), create a worktree
  (`worktrees.create`, which runs git on a caller-named repo), open a worktree
  or an editor, write files from outside the sandbox (`search.replace`,
  `skills.seed`), run tools or presets, or change a worktree's sandbox profile.
- **May not** enqueue or drain the merge queue (`merge.add`, `merge.drain`):
  the Lead lands only through a rule's land step or an owner approval. In
  this release a rule's `merge` autonomy lands unattended only with a sealed
  merge gate (see "The merge gate"); otherwise it behaves like `review`.
- **May** read everything a read token can, kill sessions, record a session,
  file folders, work the tracker, the dispatch roster
  (without binding a session), notifications, approvals requests, its own
  automation rules, and read the merge queue.

A Lead-authored automation rule that opens a session is held to the same
rule when it fires (and the worker it opens counts as Lead-opened). Every
refusal is logged on `thegn::control::audit` with the outcome `lead_confined`
and the Lead's label; Lead input is logged allowed or refused with the label
and the session id, never the text.

The Lead also has a private MCP dir, `$XDG_STATE_HOME/thegn/lead-harness/mcp`,
writable, for package caches and MCP tokens. Your `~/.mcp-auth` and `~/.npmrc`
are masked. `thegn lead login --mcp <server>` fills it from a host login that
the Lead cannot influence.

## The merge gate

By default the merge queue's gate runs on the host. `[merge_queue] gate_sandbox
= "sealed"` (global or trusted-profile config only, never a repo overlay) runs
`gate_setup_command`, `gate_command` and `regenerate_command` in the repo's
sealed sandbox instead. Not yet verified live.

- **Writable:** the gate checkout and its target dir, nothing else. A
  configured `gate_target_dir` is ignored.
- **Read-only:** the repo's git common dir and the profile's own mounts.
- **Hidden:** the daemon socket, state and runtime dirs, the Lead control dir
  and `$HOME`; no `SSH_AUTH_SOCK`, no forge, tracker or model tokens. The
  environment is an allowlist.
- **Network:** none. A land that needs a download goes red with a log headed
  "sandboxed gate (network: none)" and never lands.
- **Backend:** local bwrap only. A sandbox with the Nix daemon socket or any
  socket or device mount is refused, so `nix develop` gates cannot run sealed.

If the sandbox cannot start or is refused, the gate is an infrastructure hold
that asks for approval; it is never run on the host and never blamed on the
branch. The fold itself stays thegn's own git. An unattended drain never hands
a red gate or conflict to the fixer agent.

## Remote panes

A pane in a provider sandbox is a thegn terminal like any other. Its terminal
identity, what closing it does and what runs after its prompt are the same for
every remote provider; see "Interactive remote panes" in [[environments]].

## Inspecting

To see which backend would launch for a worktree, and which layer chose each
setting, run `thegn info` or press `I` on its sidebar row (see the Info screen section of
Workspaces and worktrees).

The panel's **system → sandbox** section shows the live state for the
focused worktree: the backend, image, mounts, and the
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

`[sandbox] network_audit = true` records the podman `network` event stream
(container network attach and detach) in the sandbox audit table. It works on
podman backends only, `thegn config validate` warns when no selectable backend
can deliver it, and it is not a DNS-query or connection log.

The machine-global view is the system monitor's **Containers** tab
([[system-monitor]]), which lists every backend's containers with live
stats and lifecycle actions on the thegn-owned ones.

For scripts or a terminal without the TUI, the same local estate is available
through the CLI:

- `thegn sandbox ls` lists running containers with owned rows first;
  `--stats` adds one bounded, non-streaming sample and `--json` emits one
  stable JSON array.
- `thegn sandbox ctl stop|start|restart <name>` operates only on a
  `thegn-` owned container. It asks for confirmation unless `--yes` is given;
  foreign names fail without invoking a runtime.
- `thegn sandbox logs <name> [--tail N]` prints a bounded, non-following tail
  (up to 10,000 lines).

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
