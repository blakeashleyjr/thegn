# Harden the remote Sprites journey

## Why

Sprites are paid and thegn could create and delete them without a record of
ownership, bill idle panes, and create sprites in the background. Batch 160
makes ownership provable and bounds spend.

## What Changes

- A file-ledger record is written before a sprite is created and marked ready
  after. thegn deletes only sprites this host recorded. Pre-existing and
  foreign sprites are never deleted by thegn.
- `thegn env reap --provider sprites [--apply] [--yes] [--json]`.
- A headless keep-awake hold with a ceiling; interactive panes get none.
- `eager` never creates a sprite for an unfocused worktree.
- `min_ram_gb` sets the sprite memory limit; `size` and friends warn.
- A Sprites block in `fleet cost`; `agent run --wait --pull`.
- A first conformance suite in `thegn-svc` (`provider_conformance.rs`).

## Impact

- **Roadmap**: BB (Sandbox + remote programme), item 767, WS08-P1-08 first slice.
- Affected specs: `sandbox` (remote providers).
- No new config key, no schema bump, no new crate.

## Unverified

Nothing here has run against the real Sprites or Vercel APIs. Each is an
unticked task in `tasks.md` (THE-2234):

- The Sprites memory-policy endpoint accepts `limit_mb` with `autoscale` unset.
- The Tasks API hold keeps a sprite awake for the full ceiling.
- `GET /v1/sprites/{name}` returns 404 for a deleted sprite.
- `GET /v1/usage/{name}` field units (CPU hours, reserved GB-hours).
- The API `created_at` is present on every sprite.
- A Vercel stopped sandbox resumes on pane open.
