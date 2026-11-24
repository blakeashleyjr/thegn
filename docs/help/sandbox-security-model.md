---
id: sandbox-security-model
title: Sandbox security model
parent: sandboxing
---

# Sandbox security model

Part of [[sandboxing]]. This page states what the sandbox protects, what it does
not, and which launches still run on your host. It describes thegn at the first
sandbox-hardening wave. A row marked ⚠ is a gap: it is not true yet, and it names
the item that closes it.

## Posture

- `open` is not a sandbox.
- `standard` (the default) is an **accident guard**. It stops writes outside the
  worktree and approved paths and masks thegn's control plane. It is **not a
  boundary against a hostile agent**: home is readable, network is direct by
  default, and the kernel is shared.
- `sealed` is the hardened tier. `sealed` **with** `workspace.mode =
"private-copy"` is the boundary tier on a shared kernel.
- VM tiers add a guest kernel.

`workspace.mode = "private-copy"` is not available yet. ⚠ No private copy exists
today (WS11-P3-06), so `sealed` is the hardened tier and not yet the boundary
tier.

## What thegn protects

- **Your secrets**: `~/.ssh`, cloud credentials, the `gh` token, browser
  sessions, OS keyrings, and the OAuth refresh tokens of your agents.
- **Your host's integrity**: files outside the worktree, and anything your host
  later executes: shell rc files, toolchains, caches, git hooks, agent hooks.
- **Other sessions**: keystrokes and output of other panes, and your clipboard.
- **thegn's control plane**: the daemon control socket, the state directory, and
  pairing tokens.
- **Repository integrity**: `main` and other branches, git config and hooks, and
  the object store.
- **Your network position**: the LAN, `localhost` services, and cloud metadata
  endpoints.
- **Money**: billed provider instances.
- **Your judgement**: truthful indicators and approval prompts that cannot be
  spoofed.

## Adversaries

1. A hostile agent process (primary). A prompt-injected or malicious coding agent,
   or any tool it runs, executing arbitrary code inside a pane as your user, with
   the worktree mounted and knowledge of thegn's policy.
2. A hostile repository: a committed `.thegn.toml`, `devcontainer.json`,
   `.gitattributes`, `.envrc`, agent config in the worktree, or build scripts.
3. A hostile extension: a plugin, wrapper recipe, agent preset file, or wrapper
   binary you installed.
4. A compromised remote or provider.

## Out of scope

Kernel exploits against shared-kernel tiers; root on your machine; a malicious
thegn build; other processes you run unsandboxed as the same user; side channels;
physical access.

## Rules that hold on every tier

- A repository can only tighten policy.
- Every failure to set up enforcement refuses the launch. There is no silent
  downgrade below your isolation floor (default: shared kernel).
- The isolation class and egress label shown are derived from what actually
  launched.
- Secrets are not placed in argv, and logs are redacted.
- A repository's code runs on your host when you or thegn run it there: the merge
  gate, queue agents, test discovery, and language servers. ⚠ Until batch jobs run
  sandboxed, landing a sandbox-written branch runs its code on your host
  (WS11-P3-12, WS11-P3-13).

## Profiles

| Profile                 | Posture                           | Limit                                   |
| ----------------------- | --------------------------------- | --------------------------------------- |
| `open`                  | Not a sandbox.                    | No hardening.                           |
| `standard`              | Accident guard.                   | Not a boundary against a hostile agent. |
| `sealed`                | Hardened tier.                    | Shared kernel.                          |
| `sealed` + private copy | Boundary tier on a shared kernel. | ⚠ Not available yet (WS11-P3-06).       |

`standard` does not protect:

- A readable home, including `~/.ssh`, cloud credentials, and the agent's own
  login token.
- The network. It is direct by default, including `localhost`, the LAN, and
  metadata endpoints.
- The network namespace on bwrap. With `network = "nat"` it is the host's, so
  abstract unix sockets and localhost TCP are reachable.
- ⚠ Shared read-write toolchain caches that your host later executes (`.rustup`,
  the cargo registry and git caches, pre-commit caches). WS03-P2-05 closes this
  for `standard` in W3.

`sealed` lists the shared kernel as its limit. Its package caches are private to
the worktree and start cold. Its gaps:

- ⚠ In linked mode a sandbox can rewrite the shared object store (WS11-P3-09).
- ⚠ There is no private copy of the workspace yet (WS11-P3-06).

## Tiers

| Tier                              | Notes                                                     |
| --------------------------------- | --------------------------------------------------------- |
| Host process (`backend = "none"`) | Not a sandbox. Refused under the default isolation floor. |
| bwrap                             | Shared kernel. No process-count cap yet.                  |
| OCI (podman, docker)              | Shared kernel.                                            |
| systemd                           | Shared kernel.                                            |
| Apple `container`                 | Refuses `network = "none"` with `network-unexpressible`.  |

Further tiers (Landlock, Seatbelt, MXC, microVMs, WSL) are not available yet. See
[[sandbox-backends]] for the backends and the isolation floor.

## Egress

- `network = "none"` is offline. There is no route out.
- Allow and block lists are advisory, except on Sprites, which translates them to
  its own policy API.
- A VPN sidecar is not an exclusive route.
- ⚠ There is no allowlist proxy yet (WS04).

See [[sandbox-network]].

## Git and landing

Git state is pinned read-only on every backend. ⚠ In linked mode a sandbox can
still rewrite the object store and sibling lane refs (WS11-P3-09, WS11-P3-10).

## Terminal channels

⚠ A pane can set your clipboard with OSC 52 (WS11-P5-08).

## Documented residual risks

- Shared-kernel tiers do not survive a kernel exploit.
- Data can leave to any destination the network policy allows.
- In `standard`, every file in your home directory is readable.
- Code from a sandboxed branch runs on your host when it is landed and gated,
  tested, or built there.
- Terminal channels, as above.
- Providers and remote hosts are trusted with what you send them.

## Who runs what where

Your interactive panes (worktree shells, interactive agent panes, terminal tabs,
tools you start yourself) use `standard` by default. That is an accident guard.
Untrusted and headless launches are the untrusted classes of the sandbox security
programme ([THE-791](https://linear.app/example/issue/THE-791/sandbox-security-programme-private-workspaces-enforceable-policy)).

The target default for every untrusted class is `sealed`, private copy, private
home, `offline` (or the `agent` preset for agents), and no credentials. WS11-P6-05
flips the defaults. Until then each class below runs as the W1 column says.

| Launch class                                          | W1 state                                                                                  | Target after the flip                                            |
| ----------------------------------------------------- | ----------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| `InteractiveOwner` (your panes)                       | Sandboxed, default `standard`.                                                            | Unchanged: default stays `standard`.                             |
| `QueueAgent`                                          | ⚠ Held on the host by default. Runs on the host only if you lower the floor (WS11-P3-11). | Sealed, private copy, private home, `agent` preset (WS11-P6-05). |
| `Autopilot`                                           | ⚠ Runs on the host (WS11-P3-14).                                                          | Sealed, private copy, private home, `agent` preset (WS11-P6-05). |
| `AgentRun` (`thegn agent run`, headless session open) | ⚠ Runs on the host (WS11-P3-11).                                                          | Sealed, private copy, private home, `agent` preset (WS11-P6-05). |
| `MergeGate` (`gate_command`)                          | ⚠ Runs on the host under a CPU and memory slice only (WS11-P3-12).                        | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `Regenerate`                                          | ⚠ Runs on the host (WS11-P3-12).                                                          | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `TestDiscovery`                                       | ⚠ Runs on the host (WS11-P3-12).                                                          | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `TestRun` (Tests panel)                               | ⚠ Runs on the host (WS11-P3-15).                                                          | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `RepoLifecycle` (repo lifecycle scripts)              | ⚠ Runs on the host (WS11-P3-16).                                                          | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `HeadlessTool` (headless configured tools)            | ⚠ Runs on the host (WS11-P3-16).                                                          | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `Lsp` (language servers)                              | ⚠ Runs on the host under a CPU and memory slice only (WS11-P3-13).                        | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `ProviderExec` (headless provider exec)               | ⚠ Runs outside the sandbox policy (WS08-P7-05).                                           | Sealed, private copy, private home, `offline` (WS11-P6-05).      |
| `Plugin` (untrusted plugins)                          | ⚠ Native, unsandboxed process (WS10-P3-02).                                               | Sealed, private copy, private home, `offline` (WS11-P6-05).      |

These host-trust exceptions run on the host as explicit user-owned trust, by
design:

- `UserHook`: your host hooks.
- `HostGit`: git run on the host.
- `McpServer`: MCP servers.
- `TrustedPlugin`: plugins you mark as trusted.
- `Editor`: editors.
- `Viewer`: viewers.

See [[sandbox-profiles]] to pin a profile for a worktree.
