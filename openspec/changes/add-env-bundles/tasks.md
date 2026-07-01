# Tasks

## 1. Model + compose (superzej-core)

- [x] 1.1 `[bundle.*]` schema (`Bundle` struct), `[secrets].resolvers`,
      `[workspace.<slug>].env_bundle` in `config.rs`; extend `expand_env_ref`.
- [x] 1.2 New `bundle.rs`: `ResolvedEnv` + `compose()` (precedence resolution,
      `extends` merge, account fold, config_dirs/HOME fold, blocked keys) — **unit
      tests** mirroring `account.rs` precedence + per-key override.
- [x] 1.3 Refactor `account.rs` into a consumer of `bundle::compose` (account
      selection folds through `compose`; legacy account selection preserved).

## 2. Pane wiring (host)

- [x] 2.1 `pane.rs::spawn_with_env` clear-then-allowlist base env — **cred-leak
      test** (shell pane shows bundle identity, not launching shell's tokens).
- [x] 2.2 Route every pane spawn (agent + shell) through `bundle::compose`
      (`agent.rs` launch_spec; shells via `spawn_worktree_shell_pane`).

## 3. Dotfiles + secrets (off-loop)

- [x] 3.1 Tier-2/3 managed-HOME materialization (idempotent, hash-signature) run
      at launch off the event loop — **materialize + idempotency test**.
- [x] 3.2 Secret-resolver dispatch at launch, off-loop, never persisted — **test**
      value reaches child env; failing resolver warns + skips (degrades).

## 4. .env

- [x] 4.1 Opt-in `.env` load with allowlist hash + low precedence + credential-key
      filter — **security test** (`SECRET_KEY` filtered, `FOO` loads only after allow).

## 5. UI

- [x] 5.1 Bundle switcher: `Ctrl+Alt+u` / palette (`build_bundle_palette`) binding
      the active bundle at workspace/global scope. (Status-bar chip deferred —
      needs `FrameModel` hydration plumbing; switcher is the actionable surface.)

## 6. Validate

- [x] 6.1 `cargo test` (core+host) green; `cargo clippy --workspace` clean;
      `config.toml.example` documents `[bundle.*]`/`[secrets.resolvers]`/`env_bundle`.
