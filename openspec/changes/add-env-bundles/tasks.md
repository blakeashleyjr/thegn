# Tasks

## 1. Model + compose (superzej-core)

- [ ] 1.1 `[bundle.*]` schema (`Bundle` struct), `[secrets].resolvers`,
      `[workspace.<slug>].env_bundle` in `config.rs`; extend `expand_env_ref`.
- [ ] 1.2 New `env.rs`: `ResolvedEnv` + `compose()` (precedence resolution,
      `extends` merge, account fold, config_dirs/HOME fold, blocked keys) — **unit
      tests** mirroring `account.rs` precedence + per-key override.
- [ ] 1.3 Refactor `account.rs` into a consumer of `env::compose` (account
      selection becomes a bundle field).

## 2. Pane wiring (host)

- [ ] 2.1 `pane.rs::spawn_with_env` clear-then-allowlist base env — **cred-leak
      test** (shell pane shows bundle identity, not launching shell's tokens).
- [ ] 2.2 Route every pane spawn (agent + shell) through `env::compose`.

## 3. Dotfiles + secrets (off-loop)

- [ ] 3.1 Tier-2/3 managed-HOME materialization on a background thread (idempotent,
      channel + waker) — **idle-CPU test** (no main-loop wakeups).
- [ ] 3.2 Secret-resolver dispatch at launch, off-loop, never persisted — **test**
      value reaches child env, absent from stores/logs; failing resolver warns + skips.

## 4. .env

- [ ] 4.1 Opt-in `.env` load with allowlist hash + low precedence + credential-key
      filter — **security test** (`SECRET_KEY` filtered, `FOO` loads only after allow).

## 5. Validate

- [ ] 5.1 Run `just ci` (includes `openspec-validate`).
