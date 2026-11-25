---
id: wsl2
title: Windows (WSL2)
parent: upgrading-and-uninstalling
order: 45
---

# Windows (WSL2)

thegn has no Windows binary. On Windows, run the Linux x86_64 build inside
WSL2. The steps below use Ubuntu 24.04 and the release tarball. Everything
runs in the Linux distribution: thegn, git, your repositories and the sandbox.
Only the terminal window is a Windows program.

WSL2 is not yet verified by the maintainers. Treat this page as the install
procedure for the Linux build under WSL2, not as a tested Windows path. For
the platform matrix, see [[known-limitations]].

## 1. Install WSL2 and Ubuntu 24.04

From an elevated Windows PowerShell:

```powershell
wsl --install -d Ubuntu-24.04
```

Reboot if Windows asks, then open the distribution once and set a Linux user.
Check the version from PowerShell with `wsl -l -v`; the distribution must show
`VERSION 2`. To convert an existing one: `wsl --set-version Ubuntu-24.04 2`.

## 2. Keep repositories on the Linux filesystem

Clone and create worktrees under your Linux home (`~/code`, for example), not
under `/mnt/c`. Files on the Windows drive cross a slow bridge, and the
sandbox's bind mounts and file permissions behave differently there. Keep
thegn's own state in the Linux home as well; the defaults already do.

## 3. Install the packages thegn uses

```sh
sudo apt update
sudo apt install -y git bubblewrap
```

`git` is required. `bubblewrap` (`bwrap`) is the sandbox backend that works
on WSL2 without extra services; the default sandbox needs one of `bwrap`,
podman or docker. The optional tools (`fzf`, `gum`, `lazygit`, `yazi`, `gh`,
`gdu`, `delta`) are listed in [[upgrading-and-uninstalling]]; install the
ones you want from Ubuntu or their upstream releases.

## 4. Install the release tarball

Run these inside the Ubuntu shell. They are the same commands as the README's
prebuilt-binary section, and they verify the checksum and the GitHub
attestation before installing:

```sh
TAG="$(gh release list --repo blakeashleyjr/thegn --exclude-drafts --limit 100 --json tagName --jq '.[0].tagName')"
TARGET=x86_64-unknown-linux-gnu
ARCHIVE="thegn-${TAG}-${TARGET}.tar.gz"
CHECKSUM="thegn-${TAG}-${TARGET}.sha256"
gh release download "$TAG" --repo blakeashleyjr/thegn --pattern "$ARCHIVE" --pattern "$CHECKSUM"
sha256sum -c "$CHECKSUM"
gh attestation verify "$ARCHIVE" --repo blakeashleyjr/thegn
tar -xzf "$ARCHIVE" thegn
mkdir -p "$HOME/.local/bin"
install -m 755 thegn "$HOME/.local/bin/thegn"
```

The `gh` commands need the GitHub CLI signed in (`gh auth login`). Without it,
download the archive and its `.sha256` from the release page in a Windows
browser, then copy them into the Linux home. If you are on an older
distribution, use the musl archive instead (`x86_64-unknown-linux-musl`).

Make sure `$HOME/.local/bin` is on `PATH` (Ubuntu's default `~/.profile` adds
it when the directory exists at login; open a new shell after creating it).

## 5. Check the setup

```sh
thegn doctor
```

Read the sandbox and capabilities sections. The check should show `bwrap` as
usable. If it does not, see **bubblewrap refuses to create a namespace**
below.

## systemd

WSL2 does not start systemd unless you turn it on. thegn does **not** need it
for the default sandbox or for the pane daemon. Turn it on only if you want
one of these:

- the `systemd` sandbox backend (`backend = "systemd"`), which runs panes in
  a `systemd-run` scope;
- hard CPU caps on sandboxed work. Without a systemd user manager thegn logs
  that the hard cap was skipped and uses the soft `nice` cap instead.

To enable it, add this to `/etc/wsl.conf` inside the distribution:

```ini
[boot]
systemd=true
```

Then, from Windows PowerShell, run `wsl --shutdown` and open the distribution
again. Confirm with `systemctl --user status`; it should not report that the
user manager is unavailable.

## Windows Terminal

Use Windows Terminal with a profile that starts the distribution (the default
Ubuntu profile is fine). Some notes:

- **Fonts.** thegn draws icons and status glyphs. Set a Nerd Font in the
  profile's Appearance settings. If you see boxes or wrong glyphs, set
  `[theme] glyphs = "ascii"` in thegn's config.
- **Keys.** Windows reserves some chords before any program sees them (Win
  key combinations, Alt+Tab and similar). They never reach thegn, so rebind
  the affected action under `[keybinds]`. `thegn doctor` shows the keyboard
  row under resolved capabilities; `Ctrl-1..9` needs the extended-keys
  support described in [[troubleshooting]].
- **Colour.** Windows Terminal supports truecolour. If colours look wrong,
  check `thegn doctor` and see [[terminal-compatibility]].

## Daemon troubleshooting

thegn keeps panes alive in a pane daemon. If panes do not reattach, or a
daemon from an older build is still running after an upgrade:

1. Stop it: `thegn daemon stop`. It reports any listener it cannot stop.
2. Restart thegn from the shell you normally use.
3. Run `thegn doctor` and look at the daemon reachability line.

When the control socket cannot be placed, the daemon falls back to a private
path under the state directory, and if that cannot be secured, panes run
in-process. To run without the daemon for one session, set
`THEGN_NO_DAEMON=1`. For a persistent setting, use `[daemon] enabled = false`.
Crash reports are kept under `$XDG_STATE_HOME/thegn/crash` (usually
`~/.local/state/thegn/crash`). To collect logs, set `[log] file = true` or
`THEGN_LOG`, then reproduce the problem and read
`~/.local/state/thegn/logs/thegn.log` and `thegn-daemon.log` in the same
directory. [[debugging]] explains the support bundle.

## bubblewrap refuses to create a namespace

Ubuntu 24.04 restricts unprivileged user namespaces through AppArmor by
default. `bwrap` then fails, and thegn refuses sandboxed launches. Check:

```sh
bwrap --unshare-user --ro-bind / / -- true && echo ok
sysctl kernel.apparmor_restrict_unprivileged_userns
```

If the first command fails and the sysctl prints `1`, you can relax the
restriction system-wide with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.
That setting applies to the whole machine, so weigh it against your own risk
tolerance. Persist it in `/etc/sysctl.d/` if you choose it. You can also
keep the restriction and give `bwrap` its own AppArmor profile. Restart
thegn after either change, then run `thegn doctor` again.

If you do not want the sandbox at all, set `[sandbox] enabled = false`. See
[[sandboxing]] for what the default profile protects.

## Where things are

- Binary: `~/.local/bin/thegn`
- Config: `~/.config/thegn/config.toml`
- State: `~/.local/state/thegn/`
- Worktrees: `~/.thegn/worktrees/` by default

Upgrading and removing the binary keeps your config, state and worktrees. See
[[upgrading-and-uninstalling]].
