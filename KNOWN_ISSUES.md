# Known issues — public alpha

thegn is in public alpha. This page lists known limitations that can affect
users; it is not a complete list of bugs or planned work. See the
[`CHANGELOG.md`](CHANGELOG.md) for release changes and
[`README.md`](README.md#install) for installation instructions. Please report
reproducible problems that are not listed here.

## Supported platforms and install channels

- **Linux x86_64 is supported.** Install from the GitHub release tarball
  (GNU or musl) or the Nix flake. See [README install](README.md#install) and
  [release instructions](RELEASING.md).
- **macOS on Apple silicon is best-effort.** An unsigned, unnotarized archive
  is built, but the macOS test job is opt-in and has not completed a run; the
  on-device checklist is unfinished. The `apple` sandbox backend has unit
  tests but no automated runtime coverage, and macOS has no CPU caps.
  Set Option as Meta/Alt in your terminal; `thegn doctor` names the setting.
  See [terminal compatibility](docs/help/terminal-compatibility.md). For a
  browser-downloaded archive, clear quarantine before first launch with
  `xattr -dr com.apple.quarantine ./thegn`.
- **Windows is deferred.** No Windows binary is published.
- **Homebrew and AUR publication are pending owner setup.** They are not
  install channels yet; see [RELEASING.md](RELEASING.md#deferred-release-channels).
- Standalone `.deb` and `.rpm` assets are attached to releases, but their clean
  system install, upgrade, and uninstall rehearsal is pending. They are not a
  supported install channel. See the
  [release install-path table](RELEASING.md#install-paths-this-enables).

## UI

- **Tab-strip overflow.** When the center strip is too narrow, tabs that do
  not fit are omitted without an overflow indicator. The tabs remain available
  through navigation and return when the strip widens. Tracked in
  [`tasks.md`](tasks.md) under “Tab-strip overflow indicator.”
- **Keys typed before a pane launch spec is ready are dropped.** During lazy
  materialization, input typed before the spec arrives does not reach the new
  shell. This is most visible during sandbox or provider bring-up.

## Performance

- The startup-shell watchdog's clean-shell fallback resolves a launch spec on
  the event loop. This happens only after a new shell has produced no output by
  its deadline.

## Daemon and remote serving

- **Direct `thegn serve` traffic is plaintext.** Direct mode defaults to a
  loopback bind and refuses non-loopback plaintext unless the explicit unsafe
  override is enabled. Use a same-host TLS terminator or an encrypted tunnel
  for remote access; bearer credentials do not provide confidentiality.
- **`route_to_host` (dev channel) may hold off-host provider worktrees from
  automatic draining.** If the host cannot verify the provider source, it holds
  the row for review rather than folding it. See
  [`docs/help/merge-queue.md`](docs/help/merge-queue.md#across-hosts).
- Remote and provider surfaces remain under the deferred THE-41 audit; do not
  treat RDP, VNC, Telnet, SFTP, serial, public sharing, or multi-user parity as
  product promises.

## Concurrency

- **Active-tab persistence can race across rapid switches.** Focus changes are
  written by independent background tasks, so overlapping writes can finish
  out of order. The in-memory selection is unaffected; after a crash or forced
  kill, the restored active tab may be one switch stale. A clean quit re-persists
  the full layout.

## Security model limits

- From a source checkout, `just security-e2e` runs the sandbox-to-control
  regression suite on Linux with `bwrap` and working user namespaces. It is not
  part of `just ci` or the default CI gate.
- `sealed-tunnel` refuses to launch when a VPN is configured because thegn
  cannot yet enforce that the tunnel is the only network route. Docker Desktop
  and WSL/WSL2 sandbox binds are best-effort and unverified.
- Provider placements without an enforceable isolation request are reported as
  `provider-managed (unverified)`; their isolation is not verified by thegn.
- The `remote`, `providers`, `observe`, `placement`, and `voice` subsystems are
  dev-channel only. Their hardening and behavior are not public-alpha promises.

## Distribution

- macOS release archives are unsigned and unnotarized. A browser download is
  quarantined; remove the attribute with the command above before first launch.
- Homebrew and AUR formula/package outputs are generated, but publication is
  pending repository, environment, and key setup. The Homebrew formula is
  rendered from `packaging/homebrew/thegn.rb.tmpl` during release packaging.
- `.deb` and `.rpm` assets are unrehearsed, and crates.io / `cargo binstall`
  remain deferred. Details are in
  [RELEASING.md](RELEASING.md#install-paths-this-enables).
