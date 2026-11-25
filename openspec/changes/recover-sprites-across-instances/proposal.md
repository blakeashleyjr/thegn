# Recover a sprite across thegn instances without a manual adopt

## Why

Stable and dev keep separate ledgers. A sprite one instance created for a
worktree was refused by the other on reattach until the user ran
`thegn env adopt`.

## What changes

- thegn stamps a creation marker (`/workspace/.thegn/sprite-owner`) on a sprite
  it creates or reattaches, naming the sprite, the account fingerprint, the
  controller id and a 30 minute lease.
- When a listed sprite has no ledger record, reattach, checkpoint and restore
  read the marker and adopt the sprite automatically if it names this sprite and
  account and its lease belongs to this controller or has lapsed. A live lease
  held by another controller still refuses with the `thegn env adopt` line.
- The ledger never rewrites an existing record, so a sprite recorded by
  another controller, account or provider is never taken.

## Impact

Sprites provider reattach. No config key or schema change. Help: environments.
