# Remote daily loop

## Why

Sprites, Fly and machine0 work needs a safe daily loop: take over an existing
sprite, run, pull, land, and stop paying, without thegn ever deleting state it
cannot prove it owns.

## What Changes

- `thegn env adopt` records an existing sprite after explicit confirmation.
- `[env.<n>.provider] lifetime_reap` opts in to a bounded background reaper.
- `thegn land` pulls a provider worktree, lands the local branch and keeps the
  sandbox; `--release-sandbox` destroys it.
- Pulls are single-flight across processes.
- Sprites credentials are written 0600 and `login_homes` selects a dedicated
  login; the same key feeds the Vercel `upload` recipe.
- Provider conformance slice 2 (Fly and VPS fakes).

## Impact

- **Roadmap**: item 767 (WS08-P1-08 slice 2). Affected spec: `sandbox`.
- Two new config keys, no schema bump, no new crate.

## Unverified

Each is an unticked task in `tasks.md` (THE-2234): Claude refresh-token
sharing, Sprites checkpoint cloning, Fly IPv4 cost, pi login files.
