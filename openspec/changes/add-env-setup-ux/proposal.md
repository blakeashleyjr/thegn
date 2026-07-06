# Add self-serve environment setup (secret store + config authoring + CLI + TUI)

## Summary

Make execution environments **self-serve** — create, configure, and manage a
local/ssh/cloud environment for any repo without hand-editing TOML or exporting
env vars. Today `[env.<name>]` / `[env.<name>.provider]` are authored only by
editing `config.toml` by hand, and provider tokens are env-vars only (read
directly via `std::env::var`), so a UI that _collects_ a token has nowhere to
persist it. This change adds four layers, built bottom-up, unifying everything
the user picks as one **"Environment"** (`‹ local › ssh fly digitalocean
hetzner daytona`), the wizard branching by kind.

1. **Layered secret backend** (`superzej-core/secret.rs`) — a `SecretRef`
   resolved through a priority chain that extends the existing `env:`/`file:`
   refs with `keyring:` (OS keyring via the pure-Rust `keyring` crate) and bare
   back-compat. Writer side `secret::store` prefers the OS keyring, falls back
   to a `0600` file, and returns the ref string to put in config. **All** token
   reads route through `secret::resolve` (provider factory + `cmd/env`), so a
   collected token has a durable home and headless boxes degrade gracefully
   (keyring → file → env, never wedging a launch).
2. **Config write path** (`superzej-core/config_write.rs`) — comment-preserving
   `toml_edit` upsert: `upsert_env`/`remove_env`/`set_key`/`select_env_in_repo`
   from a typed `EnvSpec`. Repos may only _select_ an env (`.superzej.toml`
   `env = "…"`), never define one — the write path enforces the existing
   trust-clamp model (envs are global).
3. **CLI authoring** (`cmd/env.rs`, `cmd/config.rs`) — `superzej env create`
   (with `--token`/`--token-env`/`--token-file`), `env rm`, `env test` (a cheap
   provider `list()` to verify the token), and `config set <dotted.key>
<value>` — scriptable and the backing for the TUI.
4. **TUI** — an `env_wizard` modal (branches by kind; paste a token → stored;
   submit → write env + secret + optional bind) reached from the palette (New
   environment…), and a `Section::Environments` panel row (System tab) listing
   every `[env.*]` with a token-status glyph and row actions: `enter` bind to
   the current worktree, `t` test, `x` remove, `n` add.

## Impact

- tasks.md: **AE 757** (Self-serve environment setup UX); pairs with **AE 749**
  (VPS core) and `add-do-fly-providers` (the providers this authors) and reuses
  the `[env.<name>]` named-execution-environments spine.
- **superzej-core** — new `secret.rs` (feature `keyring`, pure-Rust
  `async-secret-service`/`crypto-rust` so no C `libdbus`) and `config_write.rs`
  (`toml_edit`); no DB schema change.
- **superzej-host** — `env_wizard.rs`, `env_ui.rs`, `panel/sections/
environments.rs` + `Section::Environments`; provider token reads rerouted
  through `secret::resolve`; `cmd/env.rs` (`create`/`rm`/`test`) + `cmd/
config.rs` (`set`).
- **No new event-loop wake path** — off-loop token validation (`env test` / the
  panel `t` action) runs on a spawned thread and feeds back via the existing
  refresh channel + `TerminalWaker`; the wizard modal follows the existing
  `wizard_ui` lifecycle (Full-frame damage on open/close, no idle tick).

## Rationale

The provider seam and named-execution-environments already exist; the only gap
between "superzej can run on Fly" and "a user can set that up" is authoring +
secret persistence. Layers 1–2 are independently useful (any config key becomes
writable; any token gets a safe home) and are the foundation the CLI and TUI
both reuse, so the surface stays consistent whether scripted or clicked.

## Non-goals

- **Per-repo env definitions** — repos may only _select_ an env; definitions
  stay global (trust-clamp model). The write path enforces this.
- **Secret sync across machines** — the keyring/file store is local; no cloud
  secret sync.
- **A bespoke keyring for every OS quirk** — the `keyring` crate's
  Secret-Service/macOS/Windows backends are used as-is; where none exists the
  `0600` file fallback covers it.
