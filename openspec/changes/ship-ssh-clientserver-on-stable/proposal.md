# Ship SSH remote workspaces and client/server mode on the stable channel

## Why

The owner decided that the public launch ships with full support for remote
SSH workspaces and thegn's client/server mode, alongside Sprites and Vercel.
Today the stable config clamp removes every `[env.*] placement = "ssh"` table
and clears `[sandbox.remote] host`, and the New-environment wizard hides the
`ssh` kind, so a stable user cannot reach SSH remotes.

## What changes

- `Config::clamp_to_channel` keeps `[sandbox.remote]` and SSH/mosh `[env.*]`
  tables (`placement = "ssh"`; `ssh`, `mosh` and `remote` are aliases of the
  same placement) on stable. An SSH env that also names a dev-only cloud
  provider is still removed by the provider clamp.
- Kubernetes placement stays dev-only. The `remote` feature id stays
  experimental and now names only Kubernetes placement and the direct-iroh
  listener (`serve --iroh-direct`, `pair direct`).
- Other cloud providers (Fly, DigitalOcean, Hetzner, Machine0, Daytona,
  custom) and the `[host.*]` pool are not promoted: they are separate
  provider code paths with their own lifecycle, not plain SSH hosts.
- Client/server mode (`thegn serve`, `attach`, `pair`, `session`, `events`
  over the unix or TCP control API) was never channel-gated; the docs now say
  it is stable. Direct iroh stays dev-only.
- The New-environment wizard offers `ssh` on stable. `thegn doctor` and the
  `thegn config get` clamp notice no longer claim SSH is dev-only.

## Impact

Behaviour of the stable channel for configs that name SSH remotes. No config
key, schema or migration change. Help: release-channels, environments, cli,
known-limitations; README, KNOWN_ISSUES and CHANGELOG.
