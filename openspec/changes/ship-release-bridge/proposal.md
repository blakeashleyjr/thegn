## Why

Sprites and Vercel panes need the static musl resident bridge, but release archives
did not contain it, so only Nix installs worked (THE-2668).

## What Changes

- Every release archive and package ships `thegn-musl` beside `thegn`.
- The release workflow builds the bridge once and includes it in each target archive.
- The release manifest, verification, Homebrew, AUR, deb and rpm inputs require it.
- Bridge-missing messages explain how to obtain it.
