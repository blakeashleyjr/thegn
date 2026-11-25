# Ship Sprites and Vercel environments on the stable channel

## Why

The owner decided that the public launch ships with full Sprites and Vercel
Sandbox support. Today every provider `[env.*]` is removed from the config on
the stable channel, `thegn job run` is refused, and the panel hides the
Environments section, so a stable user cannot reach either provider.

## What changes

- `Config::clamp_to_channel` keeps `[env.*]` tables whose provider is `sprites`
  or `vercel` on the stable channel (`EnvProviderKind::stable_channel`). Every
  other provider env (Fly, DigitalOcean, Hetzner, Machine0, Daytona, custom),
  the managed `[host.*]` pool, SSH/mosh, Kubernetes and the placement engine
  stay dev-only.
- `thegn job run` (and the daemon's startup reconcile of its ledger) is no
  longer a dev-channel verb: its selected runners are Sprites and Vercel only.
- The panel's system → environments section is shown on stable; the Hosts
  section stays dev-only.
- `thegn host`, `thegn fleet` and `thegn placement` stay dev-only.
- The `providers` feature id and its doctor row are unchanged: it still names
  the dev-only remainder.

## Impact

Behaviour of the stable channel for configs that name Sprites or Vercel envs.
No config key, schema or migration change. Help: release-channels,
environments, vercel-sandbox, cli, known-limitations; README and KNOWN_ISSUES.
