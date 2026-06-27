# Tasks

## 1. Startup reroot + ProfilePaths (superzej-core / host main)

- [ ] 1.1 Resolve active profile from `--profile`/`SUPERZEJ_PROFILE`; `set_var`
      the profile roots as the **first** statements in `main` (before tokio/threads).
- [ ] 1.2 `ProfilePaths` `OnceLock` typed accessor in `util.rs`; path helpers read
      the rerooted env — **unit tests**: `SUPERZEJ_PROFILE=work` ⇒ DB/logs/sockets
      under `profiles/work/`; shared base config still from real `XDG_CONFIG_HOME`.
- [ ] 1.3 First-launch migration (legacy layout → `profiles/default/`, worktrees
      left in place) — **unit test**.

## 2. Config layering (superzej-core)

- [ ] 2.1 Two-root layered load (shared base + profile + subprofile); promote
      `ProfileConfig` to a full overlay — **unit tests** for precedence.

## 3. Credential firewall (host)

- [ ] 3.1 `spawn_with_env` clear-then-allowlist; profile credential env wired
      (`GIT_CONFIG_GLOBAL`/`GH_CONFIG_DIR`/`GIT_SSH_COMMAND` IdentitiesOnly/`GNUPGHOME`);
      sandbox `~/.gitconfig` mount → profile gitconfig — **cred-leak test**.

## 4. Process + subprofile (host)

- [ ] 4.1 Per-profile `flock` singleton (one-shot, no poll); profile-launch action.
- [ ] 4.2 `Subsystem` trait + `Subsystems` holder; subprofile bind/teardown —
      test: comms switch leaves workspace untouched, no idle wakeups added.

## 5. Validate

- [ ] 5.1 Concurrency smoke: two profiles, separate DBs, zero cross-profile bleed.
- [ ] 5.2 Run `just ci` (includes `openspec-validate`).
