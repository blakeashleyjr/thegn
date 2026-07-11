# Idle Windows + Tailscale boxes as Docker hosts

Run thegn worktree sandboxes on idle Windows machines that are on your
tailnet and running Docker. **No thegn code changes are needed** — this is
configuration plus a one-time per-box setup. thegn registers each box as an
`Independent` host (never destroyed, never image-rebuilt), wraps a
`backend = "docker"` sandbox with `ssh`/`mosh` (`Placement::Ssh`), and the
container runs on that box's local Docker daemon.

## Why WSL2 (the one constraint)

thegn's remote path is deeply POSIX — it emits `/bin/sh -lc`, `command -v`,
`printf %s "$HOME"`, `md5sum`, `dtach`, `sshfs`, and `git -C` over ssh (see
`crates/thegn-core/src/placement.rs` and `remote.rs`). It cannot drive a
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
   the box sleeping/roaming; thegn auto-uses dtach when present and mosh when
   `transport = "mosh"`.
   Also optional: `apt install skopeo` for the cleanest base-image delivery
   (skopeo copies straight into the docker daemon via `docker-daemon:`).
   Without it, thegn falls back to `docker load`, which needs a reasonably
   modern Docker (Docker Desktop qualifies).
5. Worktrees are created under `~/thegn-worktrees` **in the WSL filesystem**
   (default `remote_dir`). Keep them there, not on `/mnt/c` — a git worktree
   needs a real Linux FS for symlinks/inodes/perf.

## thegn config

Edit the global config (`thegn config path`). `install_runtime = "never"`
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
`thegn host add me@win1 --name win1 --install never` (then `win2`), and
`thegn host provision win1`.

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
2. `thegn host list` shows `win1`/`win2` (reach `ssh`); `thegn host
provision win1` ends `ready`; `thegn host status win1` reports runtime
   `docker`.
3. Option B: `thegn placement plan --json` — confirm a spawn lands on
   win1/win2 and why.
4. Open a worktree with env `win1` (A) or `docker-fleet` (B): the tabbar shows
   `(mosh)`/`(ssh) [docker]`, `docker ps` on the box shows the sandbox
   container, and the worktree is under `~/thegn-worktrees`.
5. Commit and `git push` from inside the worktree — proves `forward_agent`
   carries your keys through.
6. The diff/PR panel populates over ssh exactly like a local worktree
   (`GitLoc::Remote`).

## Podman Desktop on Windows (native machine)

Podman is thegn's **preferred** runtime (probed first; the provisioner can
even install it). Podman Desktop runs podman inside a `podman-machine-default`
WSL2 VM (Fedora CoreOS), user `core`, rootless — so you can use that machine
directly as a host. Two rough edges vs a dedicated Ubuntu distro: CoreOS is
immutable (git must be layered + a restart) and it's a separate WSL VM, so the
tailnet needs one hop to reach it. `remote_dir` (`~/thegn-worktrees`) lands
on `/var/home/core`, which persists.

### 1. One-time, inside the machine

```sh
podman machine ssh              # from the Windows host
sudo rpm-ostree install git     # not preinstalled on CoreOS
sudo systemctl reboot           # or: podman machine stop && podman machine start
# back in: `git --version` and `podman info` both work
mkdir -p ~/thegn-worktrees
```

### 2. Reach it over Tailscale — pick one (both durable)

**A — Tailscale SSH inside the machine** (no keys, no port-forward; the machine
becomes its own tailnet node). Install tailscale in the machine (rpm-ostree with
the Tailscale repo, or run it as a container), then:

```sh
sudo tailscale up --ssh         # note its MagicDNS name → connect as core@<name>
```

**B — Windows-side port-forward + the machine's key** (nothing else installed in
the machine). Podman already exposes the machine's sshd on `127.0.0.1:<port>`:

```powershell
podman system connection list   # URI shows core@127.0.0.1:<PORT>
# forward the Windows Tailscale IP to it (netsh portproxy persists across reboots):
netsh interface portproxy add v4tov4 listenaddress=<WIN_TS_IP> listenport=2222 `
  connectaddress=127.0.0.1 connectport=<PORT>
netsh advfirewall firewall add rule name="sz-podman-ssh" dir=in action=allow `
  protocol=TCP localport=2222
# copy the machine key to the thegn host:
#   %USERPROFILE%\.ssh\podman-machine-default  ->  ~/.ssh/podman-machine  (chmod 600)
```

### 3. thegn config (`thegn config path`) — declarative + durable

```toml
[host.winpod]
reach = "ssh"
install_runtime = "never"       # podman is already present
[host.winpod.ssh]
# A (Tailscale SSH):  host = "core@winpod"
# B (portproxy+key):  host = "core@<win-tailscale-name>", port = 2222, identity = "~/.ssh/podman-machine"
host = "core@winpod"
transport = "ssh"               # keep ssh (CoreOS has no mosh); Tailscale SSH is ssh-only
forward_agent = true            # remote `git push` uses your local keys

[env.winpod]
placement = "ssh"
host = "winpod"
[env.winpod.sandbox]
backend = "podman"              # rootless podman as user `core`
```

Imperative equivalent (persists to the state DB instead of config):
`thegn host add core@winpod --name winpod --install never`.

### 4. Provision + verify

```sh
thegn host provision winpod          # connect → probe (podman rootless) → deliver base image → ready
thegn host status winpod             # runtime shows `podman <ver>`, rootless
ssh core@winpod -- 'git --version && podman info >/dev/null && echo ok'
```

Then open a worktree on env `winpod`: the tabbar shows `(ssh) [podman]`,
`podman ps` on the machine shows the sandbox container, and the worktree lives
under `/var/home/core/thegn-worktrees`.

**If the CoreOS friction (rpm-ostree + restart, tailscale-in-CoreOS) is more
than you want**, a dedicated Ubuntu WSL2 distro is the cleaner durable path:
`sudo apt install -y podman uidmap slirp4netns git openssh-server`, enable
systemd (`/etc/wsl.conf` `[boot] systemd=true`) + `loginctl enable-linger`, run
tailscale in the distro (`tailscale up --ssh`), and register the same way — or
just `thegn host add me@<distro> --install auto` and let thegn install
podman for you.
