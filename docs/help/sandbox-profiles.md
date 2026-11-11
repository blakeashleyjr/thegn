---
id: sandbox-profiles
title: Worktree isolation profiles
parent: sandboxing
actions: [sandbox-profile]
---

# Worktree isolation profiles

Each worktree can inherit the configured profile or pin an override. The default
is **standard**. A pin applies to new panes, agents, and queue or pipeline runs
for that worktree. Open panes keep their current profile until restarted.

| Profile    | What it means                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `standard` | Files outside the worktree are read-only. Home stays readable, including credentials, `~/.ssh`, cloud tokens, and agent CLI logins. Network and VPN attachments work. The root is read-only, new privileges are blocked, and the process limit is 512. Runtime capabilities remain at their defaults.                                                                                                                                                                                                                                                                                |
| `sealed`   | Home is replaced by a private empty home with a small shell/git config allowlist. Credentials, `~/.ssh`, agent logins, and thegn state are hidden. There is no network or VPN attachment and every Linux capability is dropped. The root is read-only, new privileges are blocked, and the process limit is 256. Local tools and offline builds work; API-backed agent CLIs and `git fetch`/`push` do not. SSH needs a remote container runtime; SSH without one, provider environments, `backend = "none"`, and run-on-host are refused. A launch that cannot be sealed is refused. |

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

Profile precedence is worktree pin, `[env.<name>]` sandbox profile, repository
and workspace configuration, global `[sandbox] profile`, then the built-in
standard default. A repository `.thegn.toml` can only tighten its profile; a
worktree pin is user-owned and takes precedence. Zone floors can raise the
effective profile for locally resolved launches.

Standalone terminal tabs and the global `[sandbox] profile` setting are not
worktree-scoped. The pin is stored in local thegn state, so it is not a security
boundary against someone who can already edit that state. See [[sandboxing]] for
the sandbox backends and containment details.
