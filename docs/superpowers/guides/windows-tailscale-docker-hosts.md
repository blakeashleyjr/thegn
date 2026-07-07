# Idle Windows + Tailscale boxes as Docker hosts

Run superzej worktree sandboxes on idle Windows machines that are on your
tailnet and running Docker. **No superzej code changes are needed** — this is
configuration plus a one-time per-box setup. superzej registers each box as an
`Independent` host (never destroyed, never image-rebuilt), wraps a
`backend = "docker"` sandbox with `ssh`/`mosh` (`Placement::Ssh`), and the
container runs on that box's local Docker daemon.

## Why WSL2 (the one constraint)

superzej's remote path is deeply POSIX — it emits `/bin/sh -lc`, `command -v`,
`printf %s "$HOME"`, `md5sum`, `dtach`, `sshfs`, and `git -C` over ssh (see
`crates/superzej-core/src/placement.rs` and `remote.rs`). It cannot drive a
PowerShell/cmd session. Rather than teach it native Windows, point the SSH
endpoint at **WSL2**: a real POSIX shell that also exposes the Windows Docker
daemon (Docker Desktop on Windows already runs its engine in a Linux VM). This
supports both "WSL-native sshd" and "native Windows OpenSSH" entry methods —
they just land in the same Linux shell.

## Per-box setup (once on each box)

Goal: `ssh me@winN` over Tailscale lands in a WSL2 Linux shell with `docker` and
`git`.

1. **WSL2 + a distro** (`wsl --install`, e.g. Ubuntu). Enable Docker Desktop's
   _WSL integration_ for the distro, or install `docker-ce` inside it. Verify in
   WSL: `docker version`, `git --version`.
2. **Make SSH land in WSL** — either:
   - _WSL-native sshd (cleanest):_ `sudo apt install openssh-server` and run
     sshd inside the distro on a reachable port.
   - _Native Windows OpenSSH server:_ set its default shell to WSL:
     ```powershell
     New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell `
       -Value 'C:\Windows\System32\wsl.exe' -PropertyType String -Force
     ```
     (or a `Match`/`ForceCommand` that `exec`s `wsl`).
3. **Tailscale** gives stable MagicDNS names — use `me@win1` / `me@win2` as the
   ssh host, not IPs.
4. **Optional resilience:** `apt install mosh dtach` in the distro. mosh survives
   the box sleeping/roaming; superzej auto-uses dtach when present and mosh when
   `transport = "mosh"`.
   Also optional: `apt install skopeo` for the cleanest base-image delivery
   (skopeo copies straight into the docker daemon via `docker-daemon:`).
   Without it, superzej falls back to `docker load`, which needs a reasonably
   modern Docker (Docker Desktop qualifies).
5. Worktrees are created under `~/superzej-worktrees` **in the WSL filesystem**
   (default `remote_dir`). Keep them there, not on `/mnt/c` — a git worktree
   needs a real Linux FS for symlinks/inodes/perf.

## superzej config

Edit the global config (`superzej config path`). `install_runtime = "never"`
because Docker is already present; `forward_agent = true` so remote `git push`
uses your local keys.

### Option A — manual, per-box (deterministic; boxes need not match)

```toml
[host.win1]
reach = "ssh"
install_runtime = "never"
[host.win1.ssh]
host = "me@win1"
transport = "mosh"        # or "ssh"
forward_agent = true

[host.win2]
reach = "ssh"
install_runtime = "never"
[host.win2.ssh]
host = "me@win2"
transport = "mosh"
forward_agent = true

[env.win1]
placement = "ssh"
host = "win1"
[env.win1.sandbox]
backend = "docker"

[env.win2]
placement = "ssh"
host = "win2"
[env.win2.sandbox]
backend = "docker"
```

DB-only shortcut (no config edit):
`superzej host add me@win1 --name win1 --install never` (then `win2`), and
`superzej host provision win1`.

### Option B — auto-spread across both (interchangeable boxes)

Declare each box's size, enable the placement engine, and leave the env
**unpinned** so the broker chooses.

```toml
[host.win1]
reach = "ssh"
install_runtime = "never"
capacity = { cpu = "8", memory = "16g" }
[host.win1.ssh]
host = "me@win1"
transport = "mosh"
forward_agent = true

[host.win2]
reach = "ssh"
install_runtime = "never"
capacity = { cpu = "8", memory = "16g" }
[host.win2.ssh]
host = "me@win2"
transport = "mosh"
forward_agent = true

[placement]
enabled = true
mode = "packed"           # bin-pack onto Ready [host.*]
pack_strategy = "spread"  # least-utilized first across win1/win2

[env.docker-fleet]
placement = "ssh"         # unpinned — broker picks win1 or win2
[env.docker-fleet.sandbox]
backend = "docker"
```

Both styles coexist: an explicit `[env.*] host =` pin always bypasses the
broker, so pinned `win1`/`win2` envs and the auto-spread `docker-fleet` env work
side by side.

## Verify

1. `ssh me@win1 -- 'uname -a && docker version && git --version'` → Linux uname
   with working Docker/Git (confirms the WSL landing).
2. `superzej host list` shows `win1`/`win2` (reach `ssh`); `superzej host
provision win1` ends `ready`; `superzej host status win1` reports runtime
   `docker`.
3. Option B: `superzej placement plan --json` — confirm a spawn lands on
   win1/win2 and why.
4. Open a worktree with env `win1` (A) or `docker-fleet` (B): the tabbar shows
   `(mosh)`/`(ssh) [docker]`, `docker ps` on the box shows the sandbox
   container, and the worktree is under `~/superzej-worktrees`.
5. Commit and `git push` from inside the worktree — proves `forward_agent`
   carries your keys through.
6. The diff/PR panel populates over ssh exactly like a local worktree
   (`GitLoc::Remote`).
