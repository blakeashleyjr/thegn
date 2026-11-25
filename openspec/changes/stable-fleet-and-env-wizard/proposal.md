## Why

Sprites and Vercel environments are stable (ship-sprites-vercel-on-stable), but
`thegn fleet` was still refused on stable and the "New environment…" wizard
offered every provider, including dev-only ones that the stable clamp would
silently drop (THE-2669, THE-2670, THE-2671).

## What Changes

- `thegn fleet list|cost|inspect` run on stable and read Sprites and Vercel
  environments only; other providers stay dev-only.
- The env wizard and the host picker's "set up …" rows offer only the kinds the
  channel allows (stable: `local`, `sprites`, `vercel`).
- Help pages describe Sprites and Vercel as supported on stable.
