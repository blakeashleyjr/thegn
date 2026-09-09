# Design

## Damage channels & event loop

The wizard modal follows the existing overlay lifecycle: open/close/keystroke
dirties `chrome` → a `Full` frame (same as `wizard_ui`); it never adds an idle
tick. Off-loop token validation (`env test`, the panel `t` action) runs on a
spawned thread and, on completion, sends a `RefreshKind::Model` on the existing
refresh channel **and pulses the `TerminalWaker`** — the loop drains it on wake
and re-renders only when dirty, honouring the ~0% idle invariant. No SQLite
schema change (the env binding reuses the existing `worktree_env` row via
`db.set_worktree_env`).

## Layer 1 — secret backend (`thegn-core/secret.rs`)

`resolve(secret_ref) -> Option<String>` matches a prefix chain:
`keyring:<service>/<account>` → OS keyring; `env:VAR` → `$VAR`; `file:PATH` →
a `0600` file; a bare string → treated as `env:` (back-compat, keeps every
existing config working). `store(name, token) -> SecretRef` tries the keyring,
falls back to a `0600` file under `$XDG_CONFIG_HOME/thegn/secrets/`, and
returns the ref to write into config. `forget(name)` removes both. All degrade
softly: a headless box with no Secret Service falls keyring → file → env and
never wedges a launch.

**Dependency constraint (hard):** the `keyring` crate MUST use
`async-secret-service` + `async-io` + `crypto-rust` (zbus, pure Rust). The
`sync-secret-service` feature pulls C `libdbus-sys`, which breaks
thegn-core's C-dep-free / cross-compile posture and its 95% coverage build.
Verified with `cargo tree -i libdbus-sys` (empty).

## Layer 2 — config write path (`thegn-core/config_write.rs`)

`toml_edit` (already a dep) for comment/format-preserving edits.
`upsert_env(scope, name, EnvSpec)` writes `[env.<name>]` +
`[env.<name>.provider]`/`.ssh` from a typed `EnvSpec` mirroring
`EnvProviderConfig`; `remove_env`, `set_key(dotted, value)`,
`select_env_in_repo`. A `subtable` helper creates nested tables idempotently.
`select_env_in_repo` is the only write allowed against a repo `.thegn.toml`
(the `env = "…"` selection); env _definitions_ refuse a repo scope, enforcing
the trust-clamp model in `config_resolve.rs`.

## Layer 3 — CLI (`cmd/env.rs`, `cmd/config.rs`)

`CreateArgs` (typed) → `create_env`: resolve/store the secret (layer 1) → write
the env (layer 2) → print. `env rm` (remove_env + secret::forget), `env test`
(build the provider via `provider_factory` + `RemoteProvider::list`, report
✓/✗), `config set` (set_key). Token is never echoed; a pasted `--token` is
stored, not written into config.

## Layer 4 — TUI (`env_wizard.rs`, `env_ui.rs`, `panel/sections/environments.rs`)

The wizard is modeled on `wizard.rs` (Field-focus, inline `cycle_row`
`‹ value ›`, `TextField`, `layer` modal). `KINDS` drives the kind cycle;
`fields()` branches by kind (cloud → token/region/size/image; ssh →
host/user/port; local → sandbox backend); `handle_key`/`handle_paste` edit the
focused field; submit builds a `CreateArgs`. `apply_outcome(outcome, slot,
model)` lives in `env_wizard` (not the run.rs key block) to keep run.rs under
its file-size ratchet ceiling.

`env_ui::env_snapshots(cfg)` builds the per-env `EnvSnapshot` (kind, region,
size, token-present) populated off-loop in `hydrate.rs` into
`model.panel.environments`. `panel_key` is the panel row dispatch: `enter`
binds (`db.set_worktree_env`), `t` tests off-loop, `x` removes
(`config_write::remove_env` + `secret::forget` + Model refresh), `n` opens the
wizard. `Section::Environments` renders each row with a status glyph (● token /
✗ missing / ● no-token) + a hint row.
