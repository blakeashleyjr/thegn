# Chunk 1 completion

Implemented the substrate-free toolchain activation seam and complete mise/asdf
declaration detection.

## Completed

- Added normalized local/remote detection for all specified config files, safe
  `conf.d/*.toml`, `MISE_ENV` variants, and idiomatic pin files while preserving
  the Nix-first provisioning tier.
- Added `[toolchain.mise].inject` with `auto`, `shims`, `env`, and `off` policy,
  schema validation, example documentation, and the intentionally pinned
  env-overlay ratchet entry.
- Added the object-safe provider seam, `Ready`/`Unavailable`/`Reserved` values,
  deterministic activation layers, bundle/devshell/provider/base PATH ordering,
  fill-only environment composition, and credential filtering.
- Added SHA-256 config-set/cache identity over worktree identity, declaration
  names and bytes, plus `mise.lock`, and a canonical redacted `mise.env` trust
  request.
- Removed the pre-existing implicit curl/trust/install script from core; explicit
  installation is now reserved for the host provider in chunk 2.
- Extended the env-overlay coverage harness so an explicitly pinned nested
  security key is tracked without bringing all nested structured config into
  env-override scope.

## Verification

- `cargo fmt --all -- --check`
- `just quick thegn-core`
- `cargo nextest run -p thegn-core envplan` — 65 passed
- `cargo nextest run -p thegn-core toolchain_activation` — 9 passed
- `cargo nextest run -p thegn-core config` — 553 passed (rerun outside the
  filesystem sandbox because an unrelated filtered DNS test requires a loopback
  socket)
- `cargo nextest run -p thegn-core env_overlay` — 8 passed
- `cargo nextest run -p thegn-core --test env_overlay_coverage` — 2 passed
- `git diff --check`

## Unverified

- Full-workspace, coverage, cross-target, smoke, CI, and e2e gates were not run,
  per the chunk dev-loop policy.
- Host provider/process, launch integration, status/doctor surfaces, and explicit
  install behavior are deferred to chunks 2 and 3.

## Commit

- `01b15d60 feat(core): add generic toolchain activation seam`
