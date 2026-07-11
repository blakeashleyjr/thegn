# Design

## Firewall = reroot the environment at startup

The codebase already reads env on every call (`xdg_state_home()`,
`xdg_config_home()`, `thegn_dir()`, `sandbox::resolve` env-passthrough,
`gh::resolve_token`). So the firewall is enforced by `std::env::set_var` of
profile-scoped roots **as the first statements in `main`**, before the tokio
runtime or any PTY reader thread — then paths, sandbox env, and token resolution
become correct for free. (`set_var` is `unsafe` under Rust 2024 → sequencing is
load-bearing.) A write-once `ProfilePaths` (`OnceLock`) is the typed accessor.

- Reroot: `THEGN_DIR`, `XDG_STATE_HOME` (DB + logs), and credential vars
  (`GIT_CONFIG_GLOBAL`, `GH_CONFIG_DIR`, `GH_TOKEN`, `GIT_SSH_COMMAND`,
  `GNUPGHOME`). **Do NOT** blanket-reroot `XDG_CONFIG_HOME` — the shared base
  config must still load from the real config home.

## Storage + config layering

- `profiles/<p>/{state/thegn.db, logs, activity.json, audit.log, run/,
config/git, config/gh, ssh, gnupg, comms/<subprofile>.db}`. Worktrees do **not**
  move (absolute paths baked into gitdir pointers); migration reroots DB/logs only.
- Config precedence: defaults → shared base (real XDG_CONFIG_HOME) → profile
  overlay (`profiles/<p>/config.toml`, full overlay) → subprofile overlay
  (`profiles/<p>/<subsystem>/<sub>.toml`) → existing per-workspace/env/`--set`.
- `ProfileConfig` grows from keybinds-only to a full overlay.

## Credential firewall

`pane.rs::spawn_with_env` becomes **clear-then-allowlist** (curated base + profile
creds) instead of inherit-everything. The default `~/.gitconfig:ro` sandbox mount
repoints at the profile gitconfig. `GIT_SSH_COMMAND` uses `IdentitiesOnly=yes` or
the firewall leaks agent keys. Document that `file_access=all` / `--network host`
/ `-A` defeat the firewall.

## Process & subprofile model

- Profile singleton via advisory `flock` on `profiles/<p>/run/thegn.lock`
  (`LOCK_EX|LOCK_NB`, one-shot — never a poll loop). Launch = spawn a terminal
  running `thegn --profile=X`; best-effort X11 focus via OSC-title marker.
- A `Subsystem` trait owns its storage handle, credential scope, and pane-id set.
  Subprofile switch = `teardown()` (kill its panes, drop its DB handle) →
  `bind(new_scope)`; `workspace` untouched. `bind()` does no polling; periodic
  work rides the `TerminalWaker`.

## Invariants

0% idle preserved (no poll loops, flock one-shot, subsystem periodic work on the
waker). Comms store is authoritative (not cache-over-git) — the trait makes
storage ownership explicit per subsystem. SQLite per-profile DB needs no schema
bump (separate file); the subprofile DB is its own handle.
