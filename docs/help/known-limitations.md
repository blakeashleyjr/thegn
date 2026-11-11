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
  the Nix flake; use the musl archive on systems with older glibc.
- **macOS on Apple silicon is best-effort.** The archive is unsigned; clear
  quarantine with `xattr -dr com.apple.quarantine ./thegn` after a browser
  download, set Option as Meta/Alt in your terminal, and treat CPU caps as
  unavailable; the `apple` sandbox backend has no automated runtime coverage.
  See [[terminal-compatibility]] and [[sandboxing]].
- **Windows is deferred, with no published binary.** Use a supported Linux
  build instead.
- **Homebrew, AUR, `.deb`, `.rpm`, crates.io, and `cargo binstall` are not
  install channels yet.** Use a release archive or the Nix flake.

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

## Security model

- **`sealed-tunnel` refuses to launch when a VPN is configured.** Use `sealed`
  without the tunnel requirement, or resolve the VPN configuration before
  retrying; see [[sandboxing]].
- **Docker Desktop and WSL sandbox binds are best-effort.** Treat those binds
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

- **Remote, providers, placement, and voice are dev-channel only.** Treat them
  as experimental rather than product promises; see [[release-channels]].
- **Observe and plugin app tabs are stable.** Use the stable channel for these
  features; see [[release-channels]].
