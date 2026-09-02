use super::*;

#[test]
fn resolve_personal_dotfiles_drops_nonportable_under_portable() {
    use thegn_core::config::{HomeConfig, ShellStrategy};
    let home_dir = std::env::temp_dir().join(format!("tg-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home_dir); // best-effort: test cleanup: scratch removal must never fail the test
    std::fs::create_dir_all(&home_dir).unwrap();
    // A portable file and a home-manager-style rc with absolute store paths.
    std::fs::write(home_dir.join(".gitconfig"), "[user]\n  name = x\n").unwrap();
    std::fs::write(
        home_dir.join(".zshrc"),
        "source /nix/store/abc-zsh-plugin/x.zsh\neval \"$(starship init zsh)\"\n",
    )
    .unwrap();

    let portable = HomeConfig {
        dotfiles: vec![".gitconfig".into(), ".zshrc".into()],
        strategy: ShellStrategy::Portable,
        portable_dotfiles_only: true,
        ..HomeConfig::default()
    };
    let (files, roots) = resolve_personal_dotfiles(&home_dir, &portable, "sprite");
    assert_eq!(
        files,
        vec![".gitconfig".to_string()],
        "non-portable .zshrc dropped"
    );
    assert!(
        roots.is_empty(),
        "portable strategy collects no closure roots"
    );

    // host-parity keeps everything and collects the store roots.
    let parity = HomeConfig {
        strategy: ShellStrategy::HostParity,
        ..portable.clone()
    };
    let (files, roots) = resolve_personal_dotfiles(&home_dir, &parity, "bigbox");
    assert!(
        files.contains(&".zshrc".to_string()),
        "host-parity keeps the rc"
    );
    assert!(
        roots.iter().any(|r| r.contains("zsh-plugin")),
        "roots collected: {roots:?}"
    );

    // clean uploads nothing.
    let clean = HomeConfig {
        strategy: ShellStrategy::Clean,
        ..portable.clone()
    };
    let (files, _) = resolve_personal_dotfiles(&home_dir, &clean, "sprite");
    assert!(files.is_empty(), "clean uploads no dotfiles");

    let _ = std::fs::remove_dir_all(&home_dir); // best-effort: test cleanup: scratch removal must never fail the test
}

#[test]
fn sprite_ssh_argv_wraps_proxycommand_and_remote_shell() {
    let argv = sprite_ssh_argv(
        "/usr/bin/thegn",
        "/home/me/wt",
        std::path::Path::new("/state/sprite_ed25519"),
        "sprite",
        "/workspace",
    );
    let joined = argv.join(" ");
    assert_eq!(argv[0], "ssh");
    assert!(
        joined.contains("ProxyCommand=/usr/bin/thegn sprite-proxy /home/me/wt"),
        "{joined}"
    );
    assert!(joined.contains("-i /state/sprite_ed25519"));
    assert!(joined.contains(&format!("-p {SPRITE_SSHD_PORT}")));
    assert!(argv.iter().any(|a| a == "sprite@sprite"));
    // The remote command cd's into the workdir then execs the user's login
    // shell via the probe chain (zsh first), so the host-parity rc loads.
    let remote = argv.last().unwrap();
    assert!(remote.contains("cd /workspace"), "{remote}");
    assert!(
        remote.contains("command -v zsh") && remote.contains("exec \"$tgsh\" -l"),
        "remote should run the zsh-first login chain: {remote}"
    );
}

#[test]
fn sprite_sshd_setup_script_authorizes_key_and_writes_config() {
    let s = sprite_sshd_setup_script("ssh-ed25519 AAAA... thegn-sprite");
    assert!(s.contains("authorized_keys"));
    assert!(s.contains("ssh-ed25519 AAAA")); // the pubkey is embedded (quoted)
    assert!(s.contains(&format!("Port {SPRITE_SSHD_PORT}")));
    assert!(s.contains("sprite_host_ed25519") && s.contains("sprite_sshd_config"));
}

#[test]
fn nix_copy_argv_builds_push_command() {
    let argv = nix_copy_argv(
        "s3://my-cache",
        &["/nix/store/a-foo".into(), "/nix/store/b-bar".into()],
    );
    assert_eq!(
        argv,
        vec![
            "copy".to_string(),
            "--to".to_string(),
            "s3://my-cache".to_string(),
            "/nix/store/a-foo".to_string(),
            "/nix/store/b-bar".to_string(),
        ]
    );
}

#[test]
fn devshell_push_argv_builders() {
    assert_eq!(
        nix_develop_profile_argv("/home/me/repo", "/tmp/gc", ""),
        vec![
            "develop",
            "/home/me/repo",
            "--profile",
            "/tmp/gc",
            "--command",
            "true"
        ]
    );
    assert_eq!(
        nix_develop_profile_argv("/home/me/repo", "/tmp/gc", "sandbox"),
        vec![
            "develop",
            "/home/me/repo#sandbox",
            "--profile",
            "/tmp/gc",
            "--command",
            "true"
        ]
    );
    assert_eq!(
        nix_copy_to_file_argv("/tmp/cache", "/tmp/gc"),
        vec![
            "copy",
            "--to",
            "file:///tmp/cache?compression=zstd",
            "--no-check-sigs",
            "/tmp/gc"
        ]
    );
}

#[test]
fn nix_copy_p2p_argv_targets_ssh_ng_without_sig_check() {
    let argv = nix_copy_p2p_argv("sprite", &["/nix/store/a-zsh".into()]);
    assert_eq!(&argv[0], "copy");
    assert_eq!(&argv[1], "--to");
    assert_eq!(&argv[2], "ssh-ng://sprite@sprite");
    assert!(argv.contains(&"--no-check-sigs".to_string()));
    assert!(argv.contains(&"--substitute-on-destination".to_string()));
    assert!(argv.contains(&"/nix/store/a-zsh".to_string()));
}

#[test]
fn store_root_of_truncates_to_top_level_store_path() {
    assert_eq!(
        store_root_of("/nix/store/abc-zsh-5.9.1/bin/zsh"),
        Some("/nix/store/abc-zsh-5.9.1".to_string())
    );
    assert_eq!(
        store_root_of("/nix/store/abc-zsh-5.9.1"),
        Some("/nix/store/abc-zsh-5.9.1".to_string())
    );
    assert_eq!(store_root_of("/etc/profiles/per-user/me/bin/zsh"), None);
    assert_eq!(store_root_of("/nix/store/"), None);
}

#[test]
fn native_exec_health_reports_and_recovers() {
    // Unique provider name so the process-global registry doesn't collide
    // with other tests.
    let p = "sprites-health-test-xyz";
    assert!(native_exec_healthy(p), "unseen provider starts healthy");
    native_exec_report(p, false);
    assert!(!native_exec_healthy(p), "a failure marks it unhealthy");
    native_exec_report(p, true);
    assert!(native_exec_healthy(p), "a success clears it");
}

#[test]
fn env_halt_reason_names_the_providers_own_token_var() {
    // Bug #1: a machine0 env with no explicit api_key_env must report ITS OWN
    // default token var (MACHINE0_API_KEY), not the old hardcoded SPRITES_TOKEN
    // that produced a nonsensical "sprites key" halt modal for machine0 envs.
    with_temp_state("halt-token-var", || {
        let cfg: Config = toml::from_str(
            "[env.m0]\nplacement = \"provider\"\n[env.m0.provider]\nprovider = \"machine0\"\n",
        )
        .unwrap();
        let wt = std::env::temp_dir()
            .join(format!("tg-halt-m0-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/m0", "/x/app", &wt, "tg/m0", None, None)
            .unwrap();
        db.set_worktree_env(&wt, "m0").unwrap();
        // Both candidate vars unset so the check fails on the RIGHT one.
        // SAFETY: guarded by ENV_LOCK inside with_temp_state.
        unsafe {
            std::env::remove_var("MACHINE0_API_KEY");
            std::env::remove_var("SPRITES_TOKEN");
        }
        let halt = env_halt_reason(&cfg, &wt).expect("a tokenless provider env halts");
        assert!(
            halt.reason.contains("MACHINE0_API_KEY"),
            "reason names the machine0 var: {}",
            halt.reason
        );
        assert!(
            !halt.reason.contains("SPRITES_TOKEN"),
            "no nonsensical sprites var: {}",
            halt.reason
        );
    });
}

#[test]
fn env_halt_reason_resolves_a_file_secret_ref() {
    // A `file:` (or keyring:) SecretRef must resolve like the provider does — NOT
    // be treated as a literal env-var name. Regression: a machine0 env whose token
    // lives at `file:~/.secrets/machine0/personal-key` falsely halted with
    // "$file:… is not set" because the check used std::env::var, not secret::resolve.
    with_temp_state("halt-file-secret", || {
        let tok = std::env::temp_dir().join(format!("tg-m0-token-{}", std::process::id()));
        std::fs::write(&tok, "secret-token-value\n").unwrap();
        let cfg: Config = toml::from_str(&format!(
            "[env.m0f]\nplacement = \"provider\"\n[env.m0f.provider]\nprovider = \"machine0\"\napi_key_env = \"file:{}\"\n",
            tok.display()
        ))
        .unwrap();
        let wt = std::env::temp_dir()
            .join(format!("tg-halt-m0f-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/m0f", "/x/app", &wt, "tg/m0f", None, None)
            .unwrap();
        db.set_worktree_env(&wt, "m0f").unwrap();
        // Provider healthy so only the token gate is under test.
        native_exec_report("machine0", true);
        assert!(
            env_halt_reason(&cfg, &wt).is_none(),
            "a resolvable file: token must not halt"
        );
        let _ = std::fs::remove_file(&tok); // best-effort: test cleanup: scratch removal must never fail the test
    });
}

#[test]
fn env_halt_reason_halts_ssh_provider_on_connect_failure() {
    // Bug #2: an ssh-reached provider (machine0) with its token SET but a recent
    // connection failure in the health registry raises the halt; recovery drops it.
    with_temp_state("halt-connect", || {
        let cfg: Config = toml::from_str(
            "[env.m0c]\nplacement = \"provider\"\n[env.m0c.provider]\nprovider = \"machine0\"\napi_key_env = \"TG_TEST_M0_TOKEN\"\n",
        )
        .unwrap();
        let wt = std::env::temp_dir()
            .join(format!("tg-halt-m0c-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/m0c", "/x/app", &wt, "tg/m0c", None, None)
            .unwrap();
        db.set_worktree_env(&wt, "m0c").unwrap();
        // SAFETY: guarded by ENV_LOCK inside with_temp_state.
        unsafe { std::env::set_var("TG_TEST_M0_TOKEN", "present") };

        // Token present + provider healthy ⇒ no halt.
        native_exec_report("machine0", true);
        assert!(
            env_halt_reason(&cfg, &wt).is_none(),
            "healthy provider with a token does not halt"
        );
        // A recent connect failure ⇒ halt describing the connection failure.
        native_exec_report("machine0", false);
        let halt = env_halt_reason(&cfg, &wt).expect("an unhealthy ssh provider halts");
        assert!(
            halt.reason.contains("connection failure") || halt.reason.contains("unreachable"),
            "reason describes the connect failure: {}",
            halt.reason
        );
        // Recovery clears it.
        native_exec_report("machine0", true);
        assert!(
            env_halt_reason(&cfg, &wt).is_none(),
            "a recovered provider no longer halts"
        );
        // SAFETY: guarded by ENV_LOCK inside with_temp_state.
        unsafe { std::env::remove_var("TG_TEST_M0_TOKEN") };
    });
}

#[test]
fn env_halt_reason_halts_on_a_selected_env_with_no_table() {
    // Regression ("machine0 silently fell back to local bwrap"): a worktree pinned
    // to an env name that has NO `[env.<name>]` table resolves to a Local fallback,
    // which the `is_local()` early return used to swallow. With the default
    // failover ("halt"), the dropped selection must instead raise a halt that names
    // the missing env — never a silent local shell.
    with_temp_state("halt-phantom-env", || {
        // Config with NO [env.ghost] table; global failover defaults to "halt".
        let cfg = Config::default();
        let wt = std::env::temp_dir()
            .join(format!("tg-halt-ghost-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/ghost", "/x/app", &wt, "tg/ghost", None, None)
            .unwrap();
        db.set_worktree_env(&wt, "ghost").unwrap();
        let halt = env_halt_reason(&cfg, &wt).expect("a phantom env selection halts");
        assert_eq!(halt.env_name, "ghost");
        assert!(
            halt.reason.contains("ghost") && halt.reason.contains("not defined"),
            "reason names the missing env: {}",
            halt.reason
        );
    });
}

fn cfg_with(agents: &[(&str, &str)], tools: &[(&str, &str)]) -> Config {
    let mut cfg = Config::default();
    let mk = |(n, c): &(&str, &str)| thegn_core::config::NamedCommand {
        name: n.to_string(),
        command: c.to_string(),
        hints: Vec::new(),
        provider: None,
        harness: None,
        resume: false,
        route_via_proxy: false,
        model: None,
        env: Default::default(),
        permissions: Vec::new(),
        drawer_scope: None,
        drawer_cwd: None,
    };
    cfg.agents = agents.iter().map(mk).collect();
    cfg.tools = tools.iter().map(mk).collect();
    cfg
}

#[test]
fn provisioned_agent_kinds_derive_from_picker() {
    // Mirrors a real picker: managed Agent (provider pi) + claude + hermes +
    // codex + a vanilla-pi npx entry + a shell. Kinds dedup; shell is skipped.
    let mut cfg = cfg_with(
        &[
            ("shell", "__shell__"),
            ("Agent", "PI_CODING_AGENT_DIR=x exec /a/pi"),
            ("claude", "claude"),
            ("hermes", "hermes"),
            ("codex", "codex"),
            ("Vanilla Pi", "npx -y @earendil-works/pi-coding-agent"),
        ],
        &[],
    );
    // Explicit providers (as the real config sets) drive the pi/claude/codex kinds.
    for (name, prov) in [
        ("Agent", "pi"),
        ("claude", "claude"),
        ("codex", "codex"),
        ("Vanilla Pi", "pi"),
    ] {
        if let Some(a) = cfg.agents.iter_mut().find(|a| a.name == name) {
            a.provider = Some(prov.to_string());
        }
    }
    let kinds = provisioned_agent_kinds(&cfg);
    assert_eq!(kinds, vec!["pi", "claude", "hermes", "codex"]); // deduped, shell skipped
    // No picker → empty (the caller then falls back to host detection).
    assert!(provisioned_agent_kinds(&Config::default()).is_empty());
}

#[test]
fn choices_lists_agents_then_tools_then_shell() {
    let cfg = cfg_with(&[("claude", "claude")], &[("lazygit", "lazygit")]);
    assert_eq!(choices(&cfg), vec!["claude", "lazygit", "shell"]);
}

#[test]
fn choices_does_not_duplicate_an_explicit_shell() {
    let cfg = cfg_with(&[], &[("shell", "bash")]);
    assert_eq!(choices(&cfg), vec!["shell"]);
}

#[test]
fn resolve_command_maps_agent_tool_and_shell() {
    let cfg = cfg_with(&[("claude", "claude --foo")], &[("lazygit", "lazygit")]);
    assert_eq!(resolve_command(&cfg, "claude"), "claude --foo");
    assert_eq!(resolve_command(&cfg, "lazygit"), "lazygit");
    assert_eq!(resolve_command(&cfg, "shell"), shell_inner(false));
    // Unknown label degrades to a shell.
    assert_eq!(resolve_command(&cfg, "nope"), shell_inner(false));
}

// Crate-wide env lock (shared with `run`'s sidebar tests): both redirect the
// process-global `XDG_STATE_HOME`, so they must serialize on the SAME mutex.
use crate::testenv::ENV_LOCK;

fn with_temp_state<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("tg-agent-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
    std::fs::create_dir_all(&dir).unwrap();
    let old = std::env::var_os("XDG_STATE_HOME");
    // SAFETY: guarded by ENV_LOCK; this module's DB-touching tests run inside this critical section.
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    let out = f();
    match old {
        Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
    }
    let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
    out
}

#[test]
fn tool_drawer_launch_is_not_recorded_as_worktree_agent() {
    with_temp_state("tool-not-agent", || {
        // A real agent + a yazi tool; host backend so launch_spec resolves.
        let mut cfg = cfg_with(&[("claude", "claude")], &[("yazi", "yazi")]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-tool-not-agent-{}", std::process::id()));
        let wt = worktree.to_string_lossy();

        // `set_worktree_agent` is UPDATE-only, so register the worktree row
        // first (as the real create path does) — otherwise every write is a
        // no-op and the test can't tell a skipped write from a matched one.
        thegn_core::db::Db::open()
            .unwrap()
            .put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
            .unwrap();

        // Launching the auto-prewarmed yazi drawer must NOT stamp the worktree.
        launch_spec(&cfg, &wt, None, "yazi").unwrap();
        let db = thegn_core::db::Db::open().unwrap();
        assert_eq!(
            db.worktree_agent(&wt).unwrap(),
            None,
            "tool drawer must not become the worktree's remembered agent"
        );

        // A real agent still records normally.
        launch_spec(&cfg, &wt, None, "claude").unwrap();
        let db = thegn_core::db::Db::open().unwrap();
        assert_eq!(
            db.worktree_agent(&wt).unwrap().as_deref(),
            Some("claude"),
            "real agents are still remembered"
        );

        // And a subsequent yazi prewarm must not clobber the real agent.
        launch_spec(&cfg, &wt, None, "yazi").unwrap();
        let db = thegn_core::db::Db::open().unwrap();
        assert_eq!(
            db.worktree_agent(&wt).unwrap().as_deref(),
            Some("claude"),
            "a later tool drawer must not overwrite the remembered agent"
        );
    });
}

#[test]
fn shell_materialize_with_suppressed_record_leaves_the_worktrees_agent_alone() {
    with_temp_state("shell-suppress", || {
        // Host backend so launch_spec resolves without a runtime.
        let mut cfg = cfg_with(&[("claude", "claude")], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-shell-suppress-{}", std::process::id()));
        let wt = worktree.to_string_lossy();

        // `set_worktree_agent` is UPDATE-only: register the worktree row
        // first, then record a wizard/`--bind`-style agent choice that the
        // shell materialize paths must not overwrite (THE-85 D4).
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
            .unwrap();
        db.set_worktree_agent(&wt, "claude").unwrap();
        drop(db);

        // The materialize/prewarm/split shell resolution passes
        // `suppress_agent_record: true`: "shell" must NOT rewrite the row.
        launch_spec_full(
            &cfg,
            &wt,
            None,
            "shell",
            false,
            false,
            LaunchExtras {
                suppress_agent_record: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            thegn_core::db::Db::open()
                .unwrap()
                .worktree_agent(&wt)
                .unwrap()
                .as_deref(),
            Some("claude"),
            "a shell materialize must not clobber the remembered agent"
        );

        // Unsuppressed (pinned old behavior): a plain "shell" launch still
        // records — the flag, not the choice, is what changed.
        launch_spec(&cfg, &wt, None, "shell").unwrap();
        assert_eq!(
            thegn_core::db::Db::open()
                .unwrap()
                .worktree_agent(&wt)
                .unwrap()
                .as_deref(),
            Some("shell"),
            "without suppression the record still happens (pinned)"
        );
    });
}

#[test]
fn prewarm_spec_leaves_the_worktrees_agent_alone() {
    with_temp_state("prewarm-no-record", || {
        // Host backend so the Ok path resolves without a runtime.
        let mut cfg = cfg_with(&[("claude", "claude")], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-prewarm-{}", std::process::id()));
        let wt = worktree.to_string_lossy();

        // `set_worktree_agent` is UPDATE-only: register the worktree row
        // first, then record a wizard/`--bind`-style agent choice that the
        // sandbox-chain pre-warm must not overwrite (THE-84).
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
            .unwrap();
        db.set_worktree_agent(&wt, "claude").unwrap();
        drop(db);

        // Ok path: the warm resolves (daemon-routed builder) and writes
        // nothing.
        prewarm_spec(&cfg, &wt).unwrap();
        assert_eq!(
            thegn_core::db::Db::open()
                .unwrap()
                .worktree_agent(&wt)
                .unwrap()
                .as_deref(),
            Some("claude"),
            "a sandbox-chain pre-warm must not clobber the remembered agent"
        );

        // Err path: the record write in `launch_spec_full` happens BEFORE the
        // sandbox resolution that fails, so even a failing warm must stay
        // inert (explicit WSL with no fallback, same shape the launch_spec
        // tests pin).
        let mut failing = cfg.clone();
        failing.sandbox.backend = thegn_core::config::SandboxBackend::Wsl;
        failing.sandbox.backend_chain = vec!["host".to_string()];
        assert!(
            prewarm_spec(&failing, &wt).is_err(),
            "explicit WSL sandbox must not degrade to host"
        );
        assert_eq!(
            thegn_core::db::Db::open()
                .unwrap()
                .worktree_agent(&wt)
                .unwrap()
                .as_deref(),
            Some("claude"),
            "even a failed pre-warm must not clobber the remembered agent"
        );
    });
}

#[test]
fn sandbox_argv_resolution_leaves_the_worktrees_agent_alone() {
    with_temp_state("argv-no-record", || {
        // Host backend so the resolution resolves. This is the exact call the
        // `sandbox-argv` verb makes (main.rs): a read-only debug verb must
        // have read-only side effects (THE-84) — the verb itself is a thin
        // CLI shell, not subprocess-tested per the crate's CLI policy.
        let mut cfg = cfg_with(&[("claude", "claude")], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        let worktree = std::env::temp_dir().join(format!("tg-agent-argv-{}", std::process::id()));
        let wt = worktree.to_string_lossy();

        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
            .unwrap();
        db.set_worktree_agent(&wt, "claude").unwrap();
        drop(db);

        launch_spec_full(
            &cfg,
            &wt,
            None,
            "shell",
            false,
            false,
            LaunchExtras {
                suppress_agent_record: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            thegn_core::db::Db::open()
                .unwrap()
                .worktree_agent(&wt)
                .unwrap()
                .as_deref(),
            Some("claude"),
            "the sandbox-argv read must not stamp the remembered agent"
        );
    });
}

#[test]
fn explicit_unavailable_sandbox_does_not_fall_back_to_host() {
    with_temp_state("explicit-no-host", || {
        let mut cfg = cfg_with(&[], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Wsl;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-wsl-missing-{}", std::process::id()));
        let err = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell")
            .expect_err("explicit WSL sandbox must not degrade to host");
        let msg = err.to_string();
        assert!(
            msg.contains("explicit sandbox backend")
                || msg.contains("refusing fallback")
                || msg.contains("could not be resolved"),
            "{msg}"
        );
    });
}

#[test]
fn auto_backend_chain_can_fall_back_to_host() {
    with_temp_state("auto-host", || {
        let mut cfg = cfg_with(&[], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-auto-host-{}", std::process::id()));
        let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
        assert_eq!(spec.backend, "host");
        assert!(spec.argv.join(" ").contains("sh"));
        assert_eq!(
            spec.warning_summary().as_deref(),
            Some("sandbox auto selected host")
        );
    });
}

#[test]
fn auto_backend_fallthrough_carries_visible_warning() {
    with_temp_state("auto-fallthrough-warning", || {
        let mut cfg = cfg_with(&[], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        // `apple` is a real (non-reserved) backend whose binary is absent on a
        // Linux box; `wsl` used to play this role but is now a reserved kind,
        // which the chain skips outright rather than probing.
        cfg.sandbox.backend_chain = vec!["apple".to_string(), "host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-auto-fallthrough-{}", std::process::id()));
        let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
        assert_eq!(spec.backend, "host");
        let warning = spec
            .warning_summary()
            .expect("host fallback should be visible");
        assert!(warning.contains("sandbox apple unavailable"), "{warning}");
        assert!(
            warning.contains("running on host after sandbox fallback"),
            "{warning}"
        );
    });
}

#[test]
fn heal_degraded_location_only_fires_for_provider_degrade() {
    // A provider env that fell back to the host this open, with a stale remote
    // blob in the DB row ⇒ heal it to local (else the chip lies "remote" while
    // the pane runs on the host — the reported machine0 bug).
    assert!(should_heal_degraded_location(true, Some("prov:machine0…")));
    // Degraded but the row is already local (empty/None) ⇒ nothing to heal.
    assert!(!should_heal_degraded_location(true, Some("")));
    assert!(!should_heal_degraded_location(true, None));
    // NOT a degrade: a genuine ssh/k8s worktree also carries `location = None`
    // from `prepare_sandbox_env` yet legitimately keeps its remote location —
    // it must never be clobbered here.
    assert!(!should_heal_degraded_location(false, Some("ssh:host…")));
    assert!(!should_heal_degraded_location(false, None));
}

#[test]
fn compose_spec_host_fallback_is_login_shell() {
    let cfg = cfg_with(&[("claude", "claude --foo")], &[]);
    let loc = GitLoc::from_db("/wt/x", None);
    let host = SandboxOutcome {
        spec: None,
        backend_label: "host".into(),
        warnings: vec!["sandbox auto selected host".into()],
        shell: String::new(),
        is_remote: false,
        cwd_override: None,
        location: None,
        degraded_from_provider: false,
    };
    let spec = compose_spec(
        &cfg,
        "/wt/x",
        Some("tg/x"),
        "claude",
        &loc,
        &host,
        LaunchExtras::default(),
    );
    assert_eq!(
        spec.argv,
        vec![
            thegn_core::util::shell(),
            "-lc".to_string(),
            "claude --foo".to_string()
        ]
    );
    assert_eq!(spec.cwd, Some(PathBuf::from("/wt/x")));
    assert!(
        spec.env
            .contains(&("THEGN_WORKTREE".to_string(), "/wt/x".to_string()))
    );
    assert!(
        spec.env
            .contains(&("THEGN_BRANCH".to_string(), "tg/x".to_string()))
    );
    // The settled backend + warnings ride into the spec.
    assert_eq!(spec.backend, "host");
    assert_eq!(
        spec.warning_summary().as_deref(),
        Some("sandbox auto selected host")
    );
}

/// OCI shell panes emit a runtime probe chain so containers that don't have
/// the host shell (e.g. a bare Debian image has bash but not zsh) still get
/// a working login shell instead of "exec: zsh: not found".
#[test]
fn shell_inner_oci_emits_runtime_probe_chain() {
    let oci = shell_inner(true);
    // Must contain a POSIX command -v probe for each candidate shell.
    assert!(
        oci.contains("command -v"),
        "should probe for shell availability"
    );
    // The `$sel` selector execs the probed shell (/bin/sh when nothing matched),
    // and the snippet ends by running that selector in the base env.
    assert!(
        oci.contains("exec \"$tgsh\" -l'") && oci.contains("${tgsh:=/bin/sh}"),
        "the $sel selector must exec the probed shell (with a /bin/sh last resort)"
    );
    assert!(
        oci.ends_with("exec sh -lc \"$sel\""),
        "must end by running the selector in the base env"
    );
    // bash must always appear in the chain (present in every Debian image).
    assert!(oci.contains("bash"), "bash must be in the probe chain");
    // Enters the flake devShell hook-independently when the workspace has an
    // `.envrc`, so a read-only-`~/.zshrc` image still enters the project
    // toolchain — via `direnv exec` (flavor-proof), never by eval'ing
    // bash-flavored export dumps in this POSIX (dash) wrapper. The login shell
    // is selected FROM INSIDE that env (the reproduced zsh lives in the devShell,
    // not the bare base PATH), so the entry runs the `$sel` selector.
    assert!(
        oci.contains("[ -e .envrc ]") && oci.contains("direnv exec . sh -lc \"$sel\""),
        "OCI shell must enter the devShell env when an .envrc is present: {oci}"
    );
    let devshell_at = oci.find("direnv exec").unwrap();
    let shell_at = oci.find("exec sh -lc \"$sel\"").unwrap();
    assert!(
        devshell_at < shell_at,
        "devShell entry must come BEFORE the base-env selector"
    );
    // Pure-devenv fallback: no `.envrc` but a `devenv.nix` + the `devenv` CLI ⇒
    // enter `devenv shell` with the probed login shell, guarded so any failure
    // falls through to the bare chain rather than killing the pane.
    assert!(
        oci.contains("[ -e devenv.nix ] && command -v devenv")
            && oci.contains("devenv shell -- sh -lc \"$sel\"")
            && oci.contains("&& exit"),
        "OCI shell must fall back to `devenv shell` for a pure-devenv repo: {oci}"
    );
    // Non-OCI: a simple "<shell> -l", not a chain.
    let host = shell_inner(false);
    assert!(
        !host.contains("command -v"),
        "host form must not emit a probe chain"
    );
    assert!(host.ends_with(" -l"), "host form must end with -l");
    assert_eq!(host, "${SHELL:-/bin/sh} -l"); // regression: ssh "exit 127"
}

#[test]
fn native_open_spec_does_not_exec_prefix_the_probe_chain() {
    // Regression: `open_spec` must not wrap the self-exec'ing probe chain in
    // another `exec`. `exec command -v zsh …` makes the shell try to exec a
    // binary named `command` (a builtin), failing with 127 and killing the
    // pane before any shell starts — the sprite "shell instantly crashes +
    // flashing splash" bug.
    let n = NativeShell {
        provider: thegn_svc::provider::Provider::Sprites(
            thegn_svc::provider::SpritesProvider::new("", "t", "s"),
        ),
        provider_name: "sprites".into(),
        sandbox_id: "s".into(),
        inner: shell_inner(true),
        workdir: "/workspace".into(),
        env: vec![],
    };
    let spec = n.open_spec(80, 24);
    let script = spec.argv.last().cloned().unwrap_or_default();
    assert!(
        !script.contains("exec command"),
        "must not exec-prefix the probe chain (127 footgun): {script}"
    );
    // The chain itself still self-execs into a shell (probed, /bin/sh last).
    assert!(script.contains("command -v zsh") && script.contains("exec \"$tgsh\" -l"));
    // And it cd's into the workdir first.
    assert!(script.starts_with("cd /workspace"));
}

#[test]
fn clean_shell_inner_is_rc_free_with_sh_fallback() {
    let clean = clean_shell_inner();
    // Plain bash is the requested fallback and must skip every startup file.
    assert!(
        clean.contains("bash --norc --noprofile"),
        "must prefer a no-rc/no-profile bash"
    );
    // The zsh middle option must use -f (NO_RCS) so a broken .zshrc can't hang.
    assert!(
        clean.contains("zsh -f"),
        "zsh fallback must skip startup files"
    );
    // Universal last resort.
    assert!(clean.ends_with("exec /bin/sh"), "must end with /bin/sh");
    // Crucially: it must NEVER run a login shell that sources the user rc.
    assert!(
        !clean.contains("-l") && !clean.contains("zsh -l") && !clean.contains("bash -l"),
        "clean fallback must not be a login shell"
    );
}

#[test]
fn compose_spec_clean_shell_choice_uses_rc_free_shell() {
    // The `clean-shell` choice composes the rc-free chain, ignoring the normal
    // login-shell path and any sandbox shell override.
    let cfg = Config::default();
    let loc = GitLoc::from_db("/wt/x", None);
    let sb = SandboxOutcome {
        spec: None, // host fallback → `$SHELL -lc <cmd>`
        backend_label: "host".into(),
        warnings: vec![],
        shell: String::new(),
        is_remote: false,
        cwd_override: None,
        location: None,
        degraded_from_provider: false,
    };
    let spec = compose_spec(
        &cfg,
        "/wt/x",
        None,
        "clean-shell",
        &loc,
        &sb,
        LaunchExtras::default(),
    );
    let joined = spec.argv.join(" ");
    assert!(
        joined.contains("bash --norc --noprofile"),
        "clean-shell argv must carry the rc-free chain, got: {joined}"
    );
}

#[test]
fn prepare_sandbox_none_backend_falls_to_host() {
    let mut cfg = Config::default();
    cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
    let loc = GitLoc::from_db("/wt/x", None);
    let out =
        prepare_sandbox_env(&cfg, Path::new("/repo"), "/wt/x", &loc, None, false, None).unwrap();
    assert!(out.spec.is_none());
    assert_eq!(out.backend_label, "host");
    // An explicit "none" choice behaves the same as the configured backend.
    let out = prepare_sandbox_env(
        &cfg,
        Path::new("/repo"),
        "/wt/x",
        &loc,
        Some("none"),
        false,
        None,
    )
    .unwrap();
    assert!(out.spec.is_none());
}

// Regression (fc68338 merge dropped `choice_is_explicit`): a fresh wizard
// pick of "host"/"none" must override a NON-"auto" config backend (e.g.
// `backend = "bwrap"`) and drop to the host shell. A non-explicit relaunch
// value must NOT — config still wins — so the two callers stay distinct.
#[test]
fn explicit_host_pick_overrides_nonauto_config() {
    let mut cfg = Config::default();
    cfg.sandbox.backend = thegn_core::config::SandboxBackend::Bwrap;
    let loc = GitLoc::from_db("/wt/x", None);
    // Fresh wizard pick (explicit) → host wins over the bwrap config.
    let out = prepare_sandbox_env(
        &cfg,
        Path::new("/repo"),
        "/wt/x",
        &loc,
        Some("host"),
        true,
        None,
    )
    .unwrap();
    assert!(out.spec.is_none(), "explicit host pick must drop to host");
    assert_eq!(out.backend_label, "host");
    // Non-explicit relaunch value against a non-"auto" config: config wins
    // (historical "explicit config beats stale DB"). bwrap may be unavailable
    // in CI, so only assert it did NOT silently become the host shell.
    let out = prepare_sandbox_env(
        &cfg,
        Path::new("/repo"),
        "/wt/x",
        &loc,
        Some("host"),
        false,
        None,
    );
    if let Ok(o) = out {
        assert_ne!(
            o.backend_label, "host",
            "non-explicit host must not beat bwrap config"
        );
    } // Err (bwrap unavailable) is acceptable — still not a host drop.
}

#[test]
fn selected_env_with_no_table_halts_or_degrades_loudly() {
    // Regression ("machine0 silently fell back to local bwrap"): selecting an env
    // that has no `[env.<name>]` table must NOT open a silent local shell.
    // failover = halt ⇒ Err(SandboxHalt); failover = auto ⇒ Ok but with
    // `degraded_from_provider` set so the notification/status/sidebar marker fire.
    with_temp_state("prep-phantom-env", || {
        let loc = GitLoc::from_db("/wt/x", None);

        // Default failover ("halt") ⇒ the dropped selection halts.
        let cfg = Config::default();
        let err = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/wt/x",
            &loc,
            None,
            false,
            Some("ghost"),
        )
        .expect_err("a phantom env selection halts when failover is off");
        let halt = err
            .downcast_ref::<crate::agent::SandboxHalt>()
            .expect("the error is a SandboxHalt");
        assert_eq!(halt.env_name, "ghost");

        // failover = auto ⇒ degrade, but LOUDLY (degraded flag set).
        let mut cfg = Config::default();
        cfg.sandbox.failover = thegn_core::config::FailoverMode::Auto;
        let out = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/wt/x",
            &loc,
            None,
            false,
            Some("ghost"),
        )
        .expect("auto failover degrades rather than halting");
        assert!(
            out.degraded_from_provider,
            "the dropped selection is flagged degraded so the notification fires"
        );
    });
}

// H1: E2E launch_spec test — backend="none" → host fallback path.
#[test]
fn launch_spec_none_backend_produces_valid_spec() {
    with_temp_state("launch-spec-none", || {
        let mut cfg = cfg_with(&[("claude", "claude --foo")], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        let worktree = std::env::temp_dir().join(format!("tg-ls-none-{}", std::process::id()));
        let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
        // Host fallback must use the login shell.
        assert!(spec.argv.join(" ").contains("sh"), "argv: {:?}", spec.argv);
        // cwd must point into the worktree.
        assert_eq!(spec.cwd, Some(worktree.clone()));
        // THEGN_WORKTREE must be injected.
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "THEGN_WORKTREE"
                    && v == &worktree.to_string_lossy().to_string()),
            "THEGN_WORKTREE missing from env"
        );
    });
}

#[test]
fn inject_devshell_host_prepends_path_and_merges_vars() {
    let dev = devenv::Devshell {
        path: Some("/nix/store/tools/bin".into()),
        vars: vec![
            ("THEGN_YAZI_BIN".into(), "/nix/store/yz/bin/yazi".into()),
            // A var the user already set on the pane must NOT be clobbered.
            ("KEEP_ME".into(), "from-devshell".into()),
        ],
    };
    let mut spec = LaunchSpec {
        argv: vec!["sh".into()],
        cwd: None,
        env: vec![("KEEP_ME".to_string(), "user-set".to_string())],
        backend: "host".into(),
        warnings: vec![],
        degraded: false,
    };
    // `inject_devshell_host` prepends to the *process* PATH, so set a known
    // base under the env guard. Without restoring it, `/usr/bin:/bin` would
    // leak to every later test, dropping git/the toolchain (under /nix/store
    // in the dev shell) out of PATH and breaking anything that shells out.
    let _env = crate::testenv::EnvVarGuard::set(&[("PATH", "/usr/bin:/bin")]);
    inject_devshell_host(&mut spec, &dev);

    let path = spec.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v);
    assert_eq!(
        path.map(String::as_str),
        Some("/nix/store/tools/bin:/usr/bin:/bin"),
        "devShell PATH must be prepended to the existing PATH"
    );
    // Only one PATH entry (any prior was replaced, not duplicated).
    assert_eq!(spec.env.iter().filter(|(k, _)| k == "PATH").count(), 1);
    // New var injected; pre-existing var preserved (not overwritten).
    assert_eq!(
        spec.env
            .iter()
            .find(|(k, _)| k == "THEGN_YAZI_BIN")
            .map(|(_, v)| v.as_str()),
        Some("/nix/store/yz/bin/yazi")
    );
    assert_eq!(
        spec.env
            .iter()
            .find(|(k, _)| k == "KEEP_ME")
            .map(|(_, v)| v.as_str()),
        Some("user-set"),
        "a var the user already set must not be clobbered"
    );
}

/// THE-91: the bundle's legacy per-provider account carve folds a credential
/// home (`CODEX_HOME = ~/.codex`) into the host pane env AFTER `compose_spec`
/// applied `[[agents]].env`. A `pipeline-*` agent that relocates that home to a
/// headless-capable config dir must keep it, or its sandbox/approval settings
/// never load and the worker dies unable to write.
#[test]
fn agent_entry_env_is_not_clobbered_by_the_host_env_fold() {
    let reserved: std::collections::BTreeMap<String, String> =
        [("CODEX_HOME".to_string(), "/pipeline/codex-home".to_string())].into();
    let mut env = vec![
        ("THEGN_WORKTREE".to_string(), "/wt".to_string()),
        // What `compose_spec` applied last from the entry.
        ("CODEX_HOME".to_string(), "/pipeline/codex-home".to_string()),
    ];

    extend_reserving(
        &mut env,
        vec![
            ("CODEX_HOME".to_string(), "/home/u/.codex".to_string()),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                "/home/u/.claude".to_string(),
            ),
        ],
        Some(&reserved),
    );

    // The reserved key is not re-appended, so the entry's value still wins.
    assert_eq!(
        env.iter().filter(|(k, _)| k == "CODEX_HOME").count(),
        1,
        "the fold must not append over a key the agent entry declares"
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == "CODEX_HOME")
            .map(|(_, v)| v.as_str()),
        Some("/pipeline/codex-home")
    );
    // Everything the entry does NOT claim still rides through.
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
            .map(|(_, v)| v.as_str()),
        Some("/home/u/.claude")
    );
}

#[test]
fn extend_reserving_without_an_agent_entry_folds_everything() {
    let mut env = vec![("THEGN_WORKTREE".to_string(), "/wt".to_string())];
    extend_reserving(
        &mut env,
        vec![("CODEX_HOME".to_string(), "/home/u/.codex".to_string())],
        None,
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == "CODEX_HOME")
            .map(|(_, v)| v.as_str()),
        Some("/home/u/.codex"),
        "a bare harness launch has no entry env to protect"
    );
}

/// The sandbox half of THE-91: `env_overrides` become `export KEY='…'` lines
/// inside the wrap script, which runs after the pane's process env is set — so
/// they clobber `[[agents]].env` just as the host fold did.
#[test]
fn agent_entry_env_is_not_clobbered_by_sandbox_overrides() {
    let reserved: std::collections::BTreeMap<String, String> =
        [("CODEX_HOME".to_string(), "/pipeline/codex-home".to_string())].into();
    let mut overrides: std::collections::HashMap<String, String> = [
        ("CODEX_HOME".to_string(), "/home/u/.codex".to_string()),
        ("SCCACHE_DIR".to_string(), "/cache/sccache".to_string()),
    ]
    .into();

    reserve_sandbox_overrides(&mut overrides, Some(&reserved));

    assert!(
        !overrides.contains_key("CODEX_HOME"),
        "a key the agent entry declares must not be re-exported inside the sandbox"
    );
    assert_eq!(
        overrides.get("SCCACHE_DIR").map(String::as_str),
        Some("/cache/sccache"),
        "unclaimed overrides still apply"
    );
}

#[test]
fn reserve_sandbox_overrides_without_an_agent_entry_keeps_everything() {
    let mut overrides: std::collections::HashMap<String, String> =
        [("CODEX_HOME".to_string(), "/home/u/.codex".to_string())].into();
    reserve_sandbox_overrides(&mut overrides, None);
    assert_eq!(
        overrides.get("CODEX_HOME").map(String::as_str),
        Some("/home/u/.codex")
    );
}

/// Reserving a credential-home key must redirect the sandbox carve, not remove
/// it: under a read-only $HOME an unmounted home makes the harness die before
/// its first turn.
#[test]
fn reserved_credential_home_is_carved_at_the_entrys_own_path() {
    let mut eff = thegn_core::agent_task::EffectiveAgent {
        name: "pipeline-coder".into(),
        command: "codex".into(),
        harness: "codex".into(),
        model: None,
        env: Default::default(),
        permissions: vec![],
        route_via_proxy: false,
    };
    eff.env
        .insert("CODEX_HOME".into(), "/home/u/.thegn/codex-pipeline".into());
    // A non-home var must not produce a mount.
    eff.env.insert("RUST_LOG".into(), "debug".into());

    let mounts = provider_home_mounts(Some(&eff));
    assert_eq!(
        mounts,
        vec![(
            "/home/u/.thegn/codex-pipeline".to_string(),
            "/home/u/.thegn/codex-pipeline".to_string()
        )],
        "only the provider home is carved, path-preserving"
    );
}

#[test]
fn provider_home_mounts_ignores_relative_and_absent_entries() {
    assert!(provider_home_mounts(None).is_empty());

    let mut eff = thegn_core::agent_task::EffectiveAgent {
        name: "x".into(),
        command: "codex".into(),
        harness: "codex".into(),
        model: None,
        env: Default::default(),
        permissions: vec![],
        route_via_proxy: false,
    };
    // A relative value is not a carvable path.
    eff.env.insert("CODEX_HOME".into(), ".codex".into());
    assert!(provider_home_mounts(Some(&eff)).is_empty());
}
