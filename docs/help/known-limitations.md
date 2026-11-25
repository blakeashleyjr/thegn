---
id: known-limitations
title: Known limitations
order: 43
---

# Known limitations

This page is the in-app list of known limitations for the public alpha. The
repository's `KNOWN_ISSUES.md` carries the same list with release and packaging
detail; if the two differ, `KNOWN_ISSUES.md` is newer.

## Platforms

- **Linux x86_64 is supported.** Install from a GNU or musl release archive or
  the Nix flake; the GNU archive needs glibc 2.28 or newer, and the musl archive works on any
  distribution.
- **macOS on Apple silicon is supported**; Intel macOS has no release binary.
  Install with `brew install blakeashleyjr/thegn/thegn` (not quarantined). The
  release archive is unsigned; clear quarantine with
  `xattr -dr com.apple.quarantine ./thegn` after a browser download. Set Option as Meta/Alt in your terminal, and treat CPU caps as
  unavailable; the sandbox on macOS is best-effort, needs Apple `container`,
  podman or docker, and the `apple` backend has no automated runtime coverage.
  See [[terminal-compatibility]] and [[sandboxing]].
- **The `apple` sandbox backend cannot run offline and has no host network.**
  `network = "none"`, and therefore the `sealed` profile, is refused with
  `network-unexpressible` because Apple's internal network still reaches
  services bound on your Mac; `network = "host"` is refused too (`nat` is the
  supported mode). Use podman or docker for offline or sealed worktrees. See
  [[sandbox-network]].
- **Windows is deferred, with no published binary.** Use the Linux build
  inside WSL2 instead; see [[wsl2]]. WSL2 is not yet verified by the
  maintainers.
- **Windows hosts: Win32-OpenSSH has no connection sharing (ControlMaster).**
  thegn opens one ssh connection per control call there. This is not verified
  on hardware yet. On any OS, a control socket path longer than 84 bytes (a
  long `THEGN_DIR` or `user@host`) also turns connection sharing off, with one
  warning; `thegn doctor` shows `multiplexing off` for such a host.
- **AUR, `.deb`, `.rpm`, crates.io, and `cargo binstall` are not install
  channels yet.** On macOS use Homebrew
  (`brew install blakeashleyjr/thegn/thegn`); on Linux use a release archive
  or the Nix flake. See [[upgrading-and-uninstalling]].

## Interface

- **The tab strip has no overflow indicator.** Use navigation to reach hidden
  tabs; they return when the strip widens.
- **Keys typed before a pane's launch spec is ready are dropped.** Wait for
  sandbox or provider setup to finish before typing into the new pane.
- **The startup-shell watchdog fallback rarely resolves a launch spec on the
  event loop, after a new shell produces no output by its deadline.** If this
  happens, use `thegn doctor` and [[debugging]] to collect diagnostics.

## Sessions

- **After a crash or forced kill, the restored active tab may be one switch
  stale.** A clean quit persists the layout again; see [[daemon-and-sessions]].

## Remote serving

- **Direct `thegn serve` traffic is plaintext.** It binds to loopback by
  default and refuses non-loopback plaintext without the explicit unsafe
  override; use a same-host TLS terminator or encrypted tunnel for remote
  access, and see [[mobile-access]].
- **`route_to_host` can hold off-host provider worktrees from automatic
  draining.** If the host cannot verify the provider source it holds the row
  for review rather than folding it; see [[merge-queue]].
- **A Sprites interactive (pty) exec can drop the line break after its first
  line of output.** The Sprite sends that first line without its CRLF, so the
  next line starts on the same row; later lines are unaffected. Non-pty execs
  keep it. This is upstream of thegn; see [[environments]].

## Security model

- **`sealed-tunnel` refuses to launch when a VPN is configured.** Use `sealed`
  without the tunnel requirement, or resolve the VPN configuration before
  retrying; see [[sandboxing]].
- **Allow/block lists (`network_allow`, `network_block`) are advisory
  everywhere, except Sprites, where they are translated to its own policy
  API.** See [[sandbox-network]].
- **A VPN sidecar routes traffic but is not an exclusive route; it does not
  confine the sandbox.** See [[sandbox-network]].
- **`network_audit` is a podman `network` event feed (attach/detach), not a
  DNS or connection log.** See [[sandbox-network]].
- **`smol`, `wsl` and `appcontainer` are reserved: accepted by config, not
  implemented, never selected.** See [[sandbox-backends]].
- **Docker Desktop sandbox binds are best-effort.** Treat those binds
  as unverified when relying on sandbox isolation; see [[sandboxing]].
- **Provider placements without an enforceable isolation request are reported
  as `provider-managed (unverified)`.** Review the provider's isolation
  guarantees before using that placement; see [[sandboxing]].
- **The default `standard` profile protects integrity, not credentials:
  `$HOME` stays readable. The opt-in `sealed` profile hides it.** See
  [[sandboxing]] and [[sandbox-profiles]].
- **Native plugins run unsandboxed as your user.** Enable only plugins you
  trust; see [[plugins]].

## Experimental subsystems

- **Kubernetes placement, other providers, placement, and voice are
  dev-channel only.** Treat them as experimental rather than product promises;
  see [[release-channels]]. Sprites and Vercel environments (and `thegn job
run` on them), SSH/mosh remote workspaces and the `thegn serve`/`attach`/
  `pair` client/server mode are the exception: they are supported on the stable
  channel. Direct Iroh (`serve --iroh-direct`, `pair direct`) stays dev-only.
  SSH remotes have no recorded paid or multi-host soak on this release, and
  mosh needs `mosh-server` on the remote host. Neither has a recorded
  paid live run on this release yet; see [[environments]] and
  [[vercel-sandbox]].
- **Observe and plugin app tabs are stable.** Use the stable channel for these
  features; see [[release-channels]].
