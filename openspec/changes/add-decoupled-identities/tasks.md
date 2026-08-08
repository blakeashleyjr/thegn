# Tasks

## 1. Identity config model (thegn-core)

- [x] 1.1 `IdentityConfig { git: {config, ssh_key}, gh: {config}, gpg: {home},
accounts: BTreeMap<String,String> }` + a named `identities` collection in
      `config.rs`; each field optional. **Unit tests**: parse a full + a partial
      identity; empty collection is the default.
- [x] 1.2 `identity::resolve(cfg, name)` + `identity::resolved(id)` → per-tool
      `Option<String>` bindings, `~` expanded. **Unit tests**: partial identity
      leaves unset tools `None`; tilde expands; `GIT_SSH_COMMAND` formatting.

## 2. Generalize the profile credential firewall (thegn-core)

- [x] 2.1 Per-tool identity resolution with fallback lands in `bundle::compose`
      (the pane-spawn seam): the active profile's `identity = "<name>"` folds as the
      lowest-precedence base, and each tool it sets overrides `GIT_CONFIG_GLOBAL` /
      `GH_CONFIG_DIR` / `GNUPGHOME` / `GIT_SSH_COMMAND` while unset tools fall back
      to the profile-root default that `reroot` pins into the process env and the
      pane inherits via `HOST_ENV_ALLOW_EXACT`. Tokens are never set by an identity;
      `GIT_SSH_COMMAND` forces `IdentitiesOnly=yes`. `profile::credential_env` is
      left unchanged (no config at reroot ⇒ it stays the profile-root fallback).
      **Unit tests**: identity-less compose is unchanged (regression); per-tool mix
      (git from profile base, gh from a bundle identity, gpg falls through); no
      token leak.
- [x] 2.2 Resolved identity dirs are mounted path-preserving via
      `ResolvedEnv.mounts` (folded by `fold_identity` → merged into the sandbox spec
      by `merge_into_spec`); `profile::sandbox_cred_mounts` still covers the
      profile-root fallback dirs. **Unit test**: an existing identity dir is mounted
      read-write path-preserving.

## 3. Bundle + switcher identity integration (thegn-core)

- [x] 3.1 `[bundle.<name>]` gains optional `identity`; `bundle::compose` folds a
      bundle-referenced identity before the bundle's own `env`/`config_dirs`/
      `accounts` (so explicit fields win), and a **directly-bound** identity (the
      switcher, `identity::set_active`) folds after the bundle chain per scope
      (worktree → workspace → global). **Unit tests**: bundle identity overrides the
      profile base per tool; explicit config_dirs override the referenced identity;
      a switched identity overrides a bundle-referenced one; worktree binding beats
      global; unknown identity ignored.

## 4. Switcher UI + feedback (host)

- [x] 4.1 `switch-identity` action + `build_identity_palette` listing
      `[identities.<name>]` (+ "no identity" clear), marking the active binding;
      selection binds at the focused scope (`identity::set_active`, workspace when a
      repo is focused else global) over the `ui_state` KV (no schema change).
      Resolution stays on the pane-spawn/compose path (off-loop); overlay repaint ⇒
      `Full`. **Host test**: palette lists/clears and marks active.
- [x] 4.2 Status-line feedback on switch (`Identity → <name>` / `Identity
cleared`), parity with the bundle/account switchers (no separate persistent
      chip — same UX as those switchers).

## 5. Docs + help

- [x] 5.1 Document `[identities.<name>]` + `[profiles.<p>].identity` +
      `[bundle.<name>].identity` in `config/config.toml.example`, with a per-tool
      mix example.
- [x] 5.2 Claim the `switch-identity` action id on the `docs/help/command-palette`
      page `actions:` frontmatter (help ratchet: every action id must be claimed) +
      switcher prose. `test/help-ratchet.txt` unchanged (no new pinned debt).

## 6. Validate

- [x] 6.1 `cargo test -p thegn-core identity:: profile:: bundle::` +
      `-p thegn-host palette:: help:: keymap::` green; targeted clippy clean.
- [ ] 6.2 `just ci` (fmt + lint + build + test + openspec-validate + coverage +
      smoke + nix-build) — run before landing.
