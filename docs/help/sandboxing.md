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

Named `[env.<name>]` execution environments — local, container, or a
cloud provider — appear in the panel's **system → environments** section
and the palette's "New environment…" wizard. `[sandbox] default_env`
picks the one new worktrees use.

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
focused worktree: backend, image, mounts, DNS filtering.

See [[config-reference]] for every `[sandbox]` key, and [[configuration]]
for how the layers combine.
