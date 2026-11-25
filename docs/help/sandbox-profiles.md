---
id: sandbox-profiles
title: Worktree isolation profiles
parent: sandboxing
actions: [sandbox-profile, zone-assign]
---

# Worktree isolation profiles

`standard` is an accident guard. It hides well-known credential files and the
repositories beside the worktree, but it is not a boundary against a hostile
agent: the rest of home is readable, network is direct by default, and the
kernel is shared.
`sealed` is the hardened tier. See [[sandbox-security-model]] for the full
posture.

Each worktree can inherit the configured profile or pin an override. The default
is **standard**. A pin applies to new panes, agents, and queue or pipeline runs
for that worktree. Open panes keep their current profile until restarted.

| Profile    | What it means                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `standard` | Files outside the worktree are read-only. Well-known credential files (`~/.ssh` private keys, cloud CLI logins, the `gh` token, `~/.netrc` and similar) and the repositories beside the worktree are hidden; the rest of home, including the agent's own login, stays readable. Agent settings in `~/.claude` and `~/.codex` are read-only; only their runtime state is writable. Network and VPN attachments work. The root is read-only, new privileges are blocked (not on Apple `container`), and the process limit is 512 on podman, docker and systemd only. Today the carved-back state directories include `~/.keychain` and `~/.claude-profiles`, which stay writable. Runtime capabilities remain at their defaults.             |
| `sealed`   | Home is replaced by a private empty home with a small shell/git config allowlist. Credentials, `~/.ssh`, agent logins, and thegn state are hidden. There is no network or VPN attachment and every Linux capability is dropped. The root is read-only, new privileges are blocked (not on Apple `container`), and the process limit is 256 on podman, docker and systemd only. Local tools work; package caches are the host's own, shared read-only (hook and compiler caches are private to the sandbox); API-backed agent CLIs and `git fetch`/`push` do not. SSH needs a remote container runtime; SSH without one, provider environments, `backend = "none"`, and run-on-host are refused. A launch that cannot be sealed is refused. |

In the Alt+w new-worktree wizard, focus **Isolation** and cycle between
standard and sealed. The wizard explains the selected profile. Headless
creation can use `thegn wt new --isolation standard` or `--isolation sealed`.

Use `thegn sandbox profile show`, `set <standard|sealed>`, or `clear` to inspect,
change, or remove a worktree override. The command defaults to the current
worktree; use `--worktree <path>` for another one. The command palette action
**Isolation profile for this worktree…** (`sandbox-profile`) opens the same
choice menu.

A `sealed` worktree on a host-backed container does not receive host `$HOME`
personal layers. The `[sandbox.home]` atuin key and session and the dotfiles are
not copied into the container, and thegn logs one warning per provisioning run
when it drops them.

A sealed container provisioned by an older thegn is scrubbed once on its next
open: the stale atuin key, session and config, and any carried dotfile that
still matches the host file, are removed. A mount point or a symlink target is
never touched, and a failed scrub is retried on the next open.

Profile precedence is worktree pin, `[env.<name>]` sandbox profile, repository
and workspace configuration, global `[sandbox] profile`, then the built-in
standard default. A repository `.thegn.toml` can only tighten its profile; a
worktree pin is user-owned and takes precedence. Zone floors can raise the
effective profile for locally resolved launches.

## Zones

A zone groups workspaces under one egress ceiling, sandbox floor and credential
sub-vault. Zones only tighten: egress is intersected down and the sandbox floor
only rises. Create zones with `thegn zone create <name>`.

The command palette action **Assign workspace to zone…** (`zone-assign`) lists
the zones for the active repository workspace. The current zone is marked
`current`, and `(none)` appears only when the workspace is already in a zone.
The picker shows at most 20 zones; for the rest, use `thegn zone assign`.

Joining a zone from no zone can only narrow, so the confirm reads "Zone `<z>`'s
egress ceiling, sandbox floor and credential sub-vault apply to new panes" and
defaults to **confirm**. Removing a workspace from its zone, or moving it to
another zone, may widen what new panes get. That confirm names the zone whose
ceilings are removed or replaced, and its cursor starts on **cancel**, so Enter
alone changes nothing. Picking the zone the workspace is already in only shows
`already in zone <z>`.

The change applies to new panes; open panes keep their current limits until
restarted. A zone deleted after the picker opened is refused with
`zone <z> no longer exists`. The home directory and plain-folder workspaces
cannot join a zone. The CLI equivalent is `thegn zone assign <zone> [<repo>]`,
with `none` to unassign.

Standalone terminal tabs and the global `[sandbox] profile` setting are not
worktree-scoped. The pin is stored in local thegn state, so it is not a security
boundary against someone who can already edit that state. See [[sandboxing]] for
the sandbox backends and containment details.
