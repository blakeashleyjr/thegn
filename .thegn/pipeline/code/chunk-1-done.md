# Chunk 1 — done (Lead-committed)

Work product: the first coder's edits to justfile, nix/package.nix,
.github/workflows/release.yml (its own verification: HM/darwin need no change —
home-manager zsh module appends $profile/share/zsh/site-functions per
NIX_PROFILES entry; XDG_DATA_DIRS carries HM + .nix-profile share dirs;
`nix fmt --ci` clean). Two headless turns ended before the commit step, so the
Lead committed the finished work verbatim.
UNVERIFIED (for the review stage): `nix build .#default` / `.#dev` completion
outputs were not built (cold-build cost); reviewer should check the installPhase
outputs or run the build.
