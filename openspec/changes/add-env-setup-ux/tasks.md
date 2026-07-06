# Tasks

## 1. core: layered secret backend

- [x] 1.1 `secret.rs` — `resolve(ref)` over `keyring:`/`env:`/`file:`/bare +
      `store`/`forget`/`keyring_available` (keyring → file fallback) —
      **unit tests**: each ref kind + fallback order + graceful no-keyring.
- [x] 1.2 `Cargo.toml`: `keyring` with `async-secret-service` + `async-io` +
      `crypto-rust` (pure Rust, no C `libdbus`); verify `cargo tree -i
libdbus-sys` empty.
- [x] 1.3 Route all token reads through `secret::resolve` — `provider_factory`
      (sprites/daytona/vps/fly) + `cmd/env::api_provider`.

## 2. core: config write path

- [x] 2.1 `config_write.rs` — `EnvSpec` + `upsert_env`/`remove_env`/`set_key`/
      `select_env_in_repo` via `toml_edit` (`subtable` helper) — **unit tests**:
      round-trip (create → re-parse → fields present, comments preserved) and
      refuse `[env.*]` in a repo file.
- [x] 2.2 `Cargo.toml`: `toml_edit` dep on `superzej-core`.

## 3. host: CLI authoring

- [x] 3.1 `cmd/env.rs` — `CreateArgs` + `create_env`; `env create`/`rm`/`test`
      actions (`env test` = `provider_factory` + `RemoteProvider::list`).
- [x] 3.2 `cmd/config.rs` — `config set <dotted.key> <value>` → `set_key`.

## 4. host: TUI wizard + panel

- [x] 4.1 `env_wizard.rs` — kind-branching modal (`KINDS`, `fields()`,
      `handle_key`, `handle_paste`, submit → `CreateArgs`) + `apply_outcome`
      (extracted from the run.rs key block to respect the ratchet ceiling) —
      **unit tests**.
- [x] 4.2 `env_ui.rs` — `EnvSnapshot`, `env_snapshots(cfg)`, `panel_key`
      (enter=bind / t=test / x=remove / n=wizard) — **unit tests**.
- [x] 4.3 `panel/sections/environments.rs` + `Section::Environments` +
      `SECTION_ORDER` (System tab, status glyph, hint row); `hydrate.rs`
      populates `model.panel.environments` off-loop.
- [x] 4.4 `run.rs` wiring: wizard modal lifecycle + paste site + palette "New
      environment…" + Environments panel dispatch (off-loop test feeds back via
      the refresh channel + waker).

## 5. Docs + validate

- [x] 5.1 `config/config.toml.example`: document the layered token sources
      (`keyring:`/`env:`/`file:`) and the `env create`/wizard flow.
- [x] 5.2 `cargo test --workspace` + clippy `-D warnings` + ratchet green.
- [ ] 5.3 `just ci` green (fmt + lint + build + test + openspec-validate +
      coverage ≥95% core + smoke + nix-build).
