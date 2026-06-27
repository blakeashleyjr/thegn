# Design

## Model

`[bundle.<name>]` in config: `env` (arbitrary k/v with `env:`/secret indirection),
`accounts` (per-provider, delegates to `account.rs`), `config_dirs` (Tier-1 env
redirection), `dotfiles` (Tier-2 materialized), `home` (Tier-3 synthetic HOME),
`dotenv` (opt-in), `extends` (composition). Bundles are declarative config;
per-scope bindings live in `ui_state` (like account pointers).

## Composition seam — `env::compose()`

New `crates/superzej-core/src/env.rs` is the single resolution point, returning
`ResolvedEnv { overrides, block, mounts }` (maps 1:1 onto existing
`SandboxSpec.{env_overrides, env_block, mounts}` — no new sandbox mechanism). It
subsumes the account-injection currently inlined in `agent::launch_spec_with_key`,
resolves the active bundle by precedence (worktree → workspace → global, layered
low→high, plus `extends`), expands secrets, and folds accounts/config_dirs/HOME.
**Called for every pane** (`choice = None` for shells), so a shell in the `work`
worktree sees the work identity.

## Dotfile tiers

Tier 1 (default): config-dir env redirection, no file ops. Tier 2 (opt-in):
materialize a dotfile tree into a managed per-bundle HOME, idempotent (hash +
re-link on change), **off the event loop** (background thread + channel, diff-watcher
pattern). Tier 3 (opt-in): set `HOME`/`XDG_*` to the managed HOME.

## .env + secrets

`.env` opt-in (`dotenv = true`), direnv-style allowlist by content hash, loads at
**low** precedence (never overrides bundle creds), and **filters** `*_TOKEN`/
`*_KEY`/`*_SECRET`/`*_PASSWORD` keys. `expand_env_ref` gains pluggable
`[secrets.resolvers]` (pass/sops/op/agenix/cmd) resolved at launch, off-loop,
never persisted; failure degrades (warn + skip).

## Prerequisite (shared with profiles)

`pane.rs::spawn_with_env` must become clear-then-allowlist (curated base env) or
the launching shell's creds leak past the bundle — the same fix the heavyweight
profile firewall needs.

## Invariants

Core resolution/precedence is unit-tested (95% gate); materialization/secret
resolution run off-loop; no polling; AI-additive (never a hard AI dependency).
