---
id: sandbox-security-model
title: Sandbox security model
parent: sandboxing
---

# Sandbox security model

Part of [[sandboxing]]. This page states what the sandbox protects, what it does
not, and which launches still run on your host. It describes thegn at the first
sandbox-hardening wave and states what is enforced today. A row marked ⚠ is a gap:
it is not true yet.

## Read this first

- `standard` hides `~/.ssh` private keys, cloud credentials, the `gh` token
  and the repositories beside the worktree. The rest of home, including agent
  logins, stays readable, and the network is open.
- `sealed` hides `$HOME` and the network but shares toolchain caches read-only.
- Interactive worktree panes are sandboxed. Headless agent runs, the queue, the
  merge gate, tests and language servers run on the host (see the table below).
- Network allow and block lists are advisory except on Sprites and Vercel (dev
  channel).

## Posture

- `open` is not a sandbox.
- `standard` (the default) is an **accident guard**. It stops writes outside the
  worktree and approved paths, masks thegn's control plane, hides well-known
  credential files and sibling repositories, and keeps agent settings
  read-only. It is **not a boundary against a hostile agent**: the rest of home
  is readable, network is direct by default, and the kernel is shared.
- `sealed` is the hardened tier. `sealed` **with** `workspace.mode =
"private-copy"` is the boundary tier on a shared kernel.
- VM tiers add a guest kernel.

`workspace.mode = "private-copy"` is not available yet. ⚠ No private copy exists
today, so `sealed` is the hardened tier and not yet the boundary
tier.

## What thegn protects

These are the goals. The gaps below say where today's enforcement falls short.

- **Your secrets**: `~/.ssh`, cloud credentials, the `gh` token, browser
  sessions, OS keyrings, and the OAuth refresh tokens of your agents.
- **Your host's integrity**: files outside the worktree, and anything your host
  later executes: shell rc files, toolchains, caches, git hooks, agent hooks.
  See the gaps for `standard` and `sealed` below.
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
  downgrade below the default isolation floor (shared kernel). An explicit
  `isolation_floor` may instead allow degrading.
- The isolation class and egress label shown are derived from what actually
  launched.
- Secrets are not placed in argv, and logs are redacted.
- A repository's code runs on your host when you or thegn run it there: the merge
  gate, queue agents, test discovery, and language servers. ⚠ Today batch jobs do
  not run sandboxed, so landing a sandbox-written branch runs its code on your
  host.

## Profiles

| Profile                 | Posture                           | Limit                                   |
| ----------------------- | --------------------------------- | --------------------------------------- |
| `open`                  | Not a sandbox.                    | No hardening.                           |
| `standard`              | Accident guard.                   | Not a boundary against a hostile agent. |
| `sealed`                | Hardened tier.                    | Shared kernel.                          |
| `sealed` + private copy | Boundary tier on a shared kernel. | ⚠ Not available yet.                    |

`standard` hides the credential locations, private `~/.ssh` entries and
sibling repositories listed in [[sandboxing]], and keeps agent settings in
`~/.claude` and `~/.codex` read-only. It does not protect:

- The rest of a readable home, including the agent's own login token, secrets
  stored outside the well-known locations, and folders that are not git
  repositories.
- ⚠ Sibling repositories on the systemd backend when a hidden path would also
  hide the worktree or a mount, and repositories that are not beside the
  worktree or its main checkout.
- The network. It is direct by default, including `localhost`, the LAN, and
  metadata endpoints.
- The network namespace on bwrap. With `network = "nat"` it is the host's, so
  abstract unix sockets and localhost TCP are reachable.
- ⚠ Shared read-write toolchain caches that your host later executes (`.rustup`,
  the cargo registry and git caches, pre-commit caches).
- ⚠ A shared `.git` that stays writable in some launches (see Git and landing).
- ⚠ On bwrap, a bind source is resolved by path when the pane starts, not
  when thegn checks it. A writable mount that contains another mount's source
  lets a sandbox redirect that bind. podman and docker re-attest bind sources
  after creation.

`sealed` lists the shared kernel as its limit. Its gaps:

- ⚠ Today sealed profiles share the host's read-write toolchain and package
  caches (`.rustup`, the cargo registry and git caches, pre-commit and nix
  caches), which your host later executes. Local tools work; the caches are warm
  and not private.
- ⚠ In linked mode a sandbox can rewrite the shared object store.
- ⚠ There is no private copy of the workspace yet.
- ⚠ On bwrap, a bind source is resolved by path when the pane starts, not
  when thegn checks it. A writable mount that contains another mount's source
  lets a sandbox redirect that bind. podman and docker re-attest bind sources
  after creation.

Process limits and privilege locking vary by backend:

- ⚠ The process-count limit (512 for `standard`, 256 for `sealed`) applies on
  podman, docker and systemd only. bwrap has none, and Apple `container` drops it.
- ⚠ Apple `container` also drops no-new-privileges.

## Tiers

| Tier                              | Notes                                                                                                              |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Host process (`backend = "none"`) | Not a sandbox. Choosing it opts out of the default isolation floor; only an explicit `isolation_floor` refuses it. |
| bwrap                             | Shared kernel. No process-count cap yet.                                                                           |
| OCI (podman, docker)              | Shared kernel.                                                                                                     |
| systemd                           | Shared kernel.                                                                                                     |
| Apple `container`                 | Refuses `network = "none"` and `network = "host"` with `network-unexpressible`.                                    |

Further tiers (Landlock, Seatbelt, MXC, microVMs, WSL) are not available yet. See
[[sandbox-backends]] for the backends and the isolation floor.

## Egress

- `network = "none"` is offline. There is no route out.
- Allow and block lists are advisory, except on Sprites and Vercel, which
  translate them to their own policy APIs.
- A VPN sidecar is not an exclusive route.
- ⚠ There is no allowlist proxy yet.

See [[sandbox-network]].

## Git and landing

On local bwrap and container launches, `[sandbox.git]` (default `commit-only`)
mounts the shared `.git` read-only apart from lane carve-outs, with `config` and
hooks read-only.

- ⚠ Remote placements and the systemd backend keep the shared `.git` writable
  apart from `config`.
- ⚠ `[sandbox.git] mode = "full"` and `file_access = all` or `host` keep it
  writable.
- ⚠ In linked mode a sandbox can still rewrite the object store and sibling lane
  refs.

## Terminal channels

⚠ A pane can set your clipboard with OSC 52.

## Documented residual risks

- Shared-kernel tiers do not survive a kernel exploit.
- Data can leave to any destination the network policy allows.
- In `standard`, every file in your home directory outside the hidden
  credential locations and sibling repositories is readable.
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
home, `offline` (or the `agent` preset for agents), and no credentials. A later change
flips the defaults. Until then each class below runs as the W1 column says.

| Launch class                                          | W1 state                                                                     | Target after the flip                               |
| ----------------------------------------------------- | ---------------------------------------------------------------------------- | --------------------------------------------------- |
| `InteractiveOwner` (your panes)                       | Sandboxed, default `standard`.                                               | Unchanged: default stays `standard`.                |
| `QueueAgent`                                          | ⚠ Held on the host by default. Runs on the host only if you lower the floor. | Sealed, private copy, private home, `agent` preset. |
| `Autopilot`                                           | ⚠ Runs on the host.                                                          | Sealed, private copy, private home, `agent` preset. |
| `AgentRun` (`thegn agent run`, headless session open) | ⚠ Runs on the host.                                                          | Sealed, private copy, private home, `agent` preset. |
| `MergeGate` (`gate_command`)                          | ⚠ Runs on the host under a CPU and memory slice only.                        | Sealed, private copy, private home, `offline`.      |
| `Regenerate`                                          | ⚠ Runs on the host.                                                          | Sealed, private copy, private home, `offline`.      |
| `TestDiscovery`                                       | ⚠ Runs on the host.                                                          | Sealed, private copy, private home, `offline`.      |
| `TestRun` (Tests panel)                               | ⚠ Runs on the host.                                                          | Sealed, private copy, private home, `offline`.      |
| `RepoLifecycle` (repo lifecycle scripts)              | ⚠ Runs on the host.                                                          | Sealed, private copy, private home, `offline`.      |
| `HeadlessTool` (headless configured tools)            | ⚠ Runs on the host.                                                          | Sealed, private copy, private home, `offline`.      |
| `Lsp` (language servers)                              | ⚠ Runs on the host under a CPU and memory slice only.                        | Sealed, private copy, private home, `offline`.      |
| `ProviderExec` (headless provider exec)               | ⚠ Runs outside the sandbox policy.                                           | Sealed, private copy, private home, `offline`.      |
| `Plugin` (untrusted plugins)                          | ⚠ Native, unsandboxed process.                                               | Sealed, private copy, private home, `offline`.      |

These host-trust exceptions run on the host as explicit user-owned trust, by
design:

- `UserHook`: your host hooks.
- `HostGit`: git run on the host.
- `McpServer`: MCP servers.
- `TrustedPlugin`: plugins you mark as trusted.
- `Editor`: editors.
- `Viewer`: viewers.

See [[sandbox-profiles]] to pin a profile for a worktree.
