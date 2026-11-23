# Windows and WSL test handoff

For a tester on a Windows machine. Read the status line first: **WSL2 is the
path that is expected to work. Native Windows is a candidate build, not a
supported platform.**

- Published build: `v0.1.0-alpha.118` (Linux x86_64, gnu and musl).
- Windows-native artifact: from the opt-in `windows` CI job only, when that run
  is green. It is a candidate, has 30-day retention, and is not an install
  channel.
- Repo claims: [KNOWN_ISSUES.md](../../../KNOWN_ISSUES.md),
  [README platform table](../../../README.md#platform-bundle-status),
  [CONTRIBUTING Windows notes](../../../CONTRIBUTING.md#windows-native-notes).

Do not file a bug as "Windows is broken" without saying which of the two paths
you ran.

---

## Path A — WSL2 (expected to work)

### 1. Install a distro

```powershell
wsl --install -d Ubuntu
wsl --update
```

In the distro:

```sh
sudo apt update && sudo apt install -y git curl ca-certificates
```

### 2. Install the released binary

Use the **musl** archive: it is static, so it does not care which distro you
picked.

```sh
curl -LO https://github.com/blakeashleyjr/thegn/releases/download/v0.1.0-alpha.118/thegn-v0.1.0-alpha.118-x86_64-unknown-linux-musl.tar.gz
curl -LO https://github.com/blakeashleyjr/thegn/releases/download/v0.1.0-alpha.118/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
tar xzf thegn-v0.1.0-alpha.118-x86_64-unknown-linux-musl.tar.gz
install -Dm755 thegn ~/.local/bin/thegn
export PATH="$HOME/.local/bin:$PATH"   # add to ~/.bashrc to keep
thegn --version
thegn doctor
```

With Nix, `nix profile install github:blakeashleyjr/thegn` is equivalent.

Keep `thegn doctor`'s output — it is the first thing to attach to a report.

### 3. Choose a sandbox backend

`thegn doctor` prints the enforcement matrix and which backend would launch.
Pick one:

- **bwrap** (simplest): `sudo apt install -y bubblewrap`.
- **docker**: either enable Docker Desktop's _WSL integration_ for this distro,
  or install `docker-ce` **inside** the distro.
- **podman**: install it inside the distro and check `podman info`.

The reserved backend name `wsl` is rejected on purpose: the sandbox is a Linux
runtime inside the distro, not a Windows-hosted one. Do not set
`backend = "wsl"`; set `docker`, `podman-rootless`, `podman-rootful`, `bwrap` or
`systemd`.

For the `systemd` backend, enable systemd in the distro:

```ini
# /etc/wsl.conf
[boot]
systemd=true
```

```powershell
wsl --shutdown
```

Then, back in the distro: `sudo loginctl enable-linger "$USER"`.

### 4. Keep work on the Linux filesystem

Create repos and worktrees under `~/…`, never under `/mnt/c/…`:

- The Windows drive is slow through the 9p mount, and git operations feel it.
- Container bind mounts resolve `-v` inside the WSL VM, so a `/mnt/c` path is
  not the same path inside the container. `thegn` detects this and refuses with
  a remedy naming WSL2; that refusal is the intended behaviour, not the bug.

### 5. Checklist

Run these in order and note the result of each. The first two are gates: if
either fails, stop and report.

| #   | Check                                       | What "pass" looks like                                                                   |
| --- | ------------------------------------------- | ---------------------------------------------------------------------------------------- |
| 1   | `thegn doctor`                              | Runs, names a backend, no panic. Attach the output.                                      |
| 2   | `thegn` in a repo with a few worktrees      | First frame renders; sidebar, masthead, statusbar visible.                               |
| 3   | Resize the Windows Terminal window hard     | No tearing, no panic, layout recomputes.                                                 |
| 4   | Open a pane, type `git status`              | Prompt appears, keystrokes echo, command output scrolls.                                 |
| 5   | `Ctrl-C` inside the pane                    | Interrupts the pane child, not thegn.                                                    |
| 6   | `exit` in the pane                          | Pane closes or is replaced; thegn stays up.                                              |
| 7   | Create a worktree and open a sandboxed pane | Pane opens inside the selected backend; no mount-check refusal for a `~`-based path.     |
| 8   | Fix one failing test in that pane           | Edits, saves and reruns; the diff panel shows the change.                                |
| 9   | Quit and relaunch                           | Session state and worktrees come back.                                                   |
| 10  | Optional: a remote provider env             | Only with your own Sprites/Vercel credentials. Never paste tokens or logs into a report. |

### 6. Report

Attach, per failure: the check number, the exact key sequence or command, the
`thegn --version`, `thegn doctor` output, `uname -a`, `cat /etc/wsl.conf`, and
which runtime (`docker`, `podman` or `bwrap`) the pane used. Add the log line if
one appeared.

Post to Linear:

- **THE-141** — real-host verification of the sandbox matrix (this is the issue
  that has been waiting for a WSL run).
- **THE-124** — WSL and Docker Desktop bind behaviour, if the mount check is
  what failed.

---

## Path B — native Windows (candidate, not supported)

No Windows binary is published. There are two ways to get one, in order of
preference.

### B1. The CI candidate artifact

The `windows` CI job builds and uploads `thegn-x86_64-pc-windows-msvc` with
30-day retention. It is opt-in, and it has not yet produced a recorded green
run, so check the run link you were given before spending time:

1. Open the run, confirm the `windows` and `windows-artifact-smoke` jobs are
   green (the run as a whole may be red because the opt-in `e2e` job is known
   stale).
2. Download the `thegn-x86_64-pc-windows-msvc` artifact and unzip `thegn.exe`.
3. Run it from Windows Terminal (not `conhost.exe`; thegn refuses `conhost` and
   says so).

### B2. Build from source

Needs `rustup`, Visual Studio Build Tools (MSVC), and Windows Terminal:

```powershell
git clone https://github.com/blakeashleyjr/thegn
cd thegn
cargo install --locked --path crates/thegn-host
```

Panes on Windows are scoped by kill-on-close Job Objects, so a cancelled
`cargo test` takes its test binaries with it.

### Checklist (either route)

These are the compositor gates from
`openspec/changes/add-windows-compositor-validation/tasks.md` §2. The first one
gates the rest.

1. `cargo run -p thegn-host --example waker_spike` — one tick per second, ~0%
   CPU between ticks, instant key echo.
2. Bare `thegn`: first frame renders (chrome + pane), PowerShell prompt
   appears, typing echoes.
3. Idle CPU ~0% in Task Manager with the compositor idle.
4. Drag-storm resize: no tearing, no panic.
5. `Ctrl-C` inside a pane interrupts the pane child, not thegn; `exit` reaches
   the pty drain.
6. `THEGN_LOG=info` plus a forced background warning: the frame stays clean and
   the line lands in `thegn-stderr.log`.
7. Launching under `conhost.exe` is refused with a Windows Terminal pointer.
8. Two `thegn daemon` launches: the second exits 0 with "already running", and a
   daemon-backed pane opens over the named pipe.
9. Unicode and border glyphs render with no ASCII fallback.

Report Windows-native results against the compositor-validation issue, not
THE-141. If a gate fails, that is expected information: it keeps the "deferred"
claim honest.

---

## What not to send

No tokens, no provider credentials, no model login material, no full
environment dumps, no transcripts containing secrets. Redact account ids from
`fleet`/cost output before attaching it.
