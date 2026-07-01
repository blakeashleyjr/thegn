# Tasks

## 1. Startup reroot + ProfilePaths (superzej-core / host main)

- [x] 1.1 Resolve active profile from `--profile`/`SUPERZEJ_PROFILE`; `set_var`
      the profile roots as the **first** statements in `main` (before tokio/threads)
      via `profile::reroot` (`main.rs`).
- [x] 1.2 `ProfilePaths` accessor + `profile.rs` typed module; path helpers read
      the rerooted env — **unit tests**: named profile ⇒ root under
      `profiles/<name>/`; shared base config still from real `XDG_CONFIG_HOME`.
- [x] 1.3 Migration intentionally omitted: the **default profile stays in place**
      (no whole-user data migration); only named profiles reroot into a fresh
      `profiles/<name>/` tree. Documented in `profile.rs` + `config.toml.example`.

## 2. Config layering (superzej-core)

- [x] 2.1 Two-root layered load: shared base + named-profile `config.toml` overlay
      from the real `XDG_CONFIG_HOME` (`apply_toml_overlay` deep-merge, below
      env/`--set`) — **unit tests** for precedence + preserved-untouched keys.

## 3. Credential firewall (host)

- [x] 3.1 `spawn_with_env` clear-then-allowlist (shared with AU); profile
      credential env wired at reroot (`GIT_CONFIG_GLOBAL`/`GH_CONFIG_DIR`/
      `GNUPGHOME`; forge tokens dropped; `GIT_SSH_COMMAND` IdentitiesOnly when a
      profile key exists); sandbox cred mounts via `profile::sandbox_cred_mounts`
      — **cred-leak + `credential_env` tests**; caveats documented.

## 4. Process + subprofile (host)

- [x] 4.1 Per-profile advisory `flock` singleton (`profile::acquire_singleton`,
      one-shot `LOCK_EX|LOCK_NB`, no poll); profile-launch action
      (`launch_window_argv` + `Ctrl+Alt+g` / palette switcher).
- [x] 4.2 `Subsystem` trait + `Subsystems` holder (`subsystem.rs`); subprofile
      bind/teardown exercised by a stub subsystem — **tests**: comms switch reaps
      its panes + rebinds storage while `workspace` is untouched; no polling.

## 5. Validate

- [x] 5.1 `cargo test` (core+host) green; `cargo clippy --workspace` clean;
      `config.toml.example` documents `--profile` + the credential firewall.
- [ ] 5.2 `just ci` (fmt + openspec-validate + coverage + smoke + nix-build) —
      run before landing.
