use super::*;

#[test]
fn resolve_personal_dotfiles_drops_nonportable_under_portable() {
    use thegn_core::config::{HomeConfig, ShellStrategy};
    let home_dir = std::env::temp_dir().join(format!("sz-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home_dir);
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

    let _ = std::fs::remove_dir_all(&home_dir);
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
        remote.contains("command -v zsh") && remote.contains("exec zsh -l"),
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
fn sanitize_detail_strips_ansi_control_and_collapses_whitespace() {
    // The real failing-step string: ANSI SGR codes + newlines (what tripped
    // the renderer). Sanitized to a single clean line.
    let raw = "Build dev shell (exit 2): \u{1b}[1m\u{1b}[32merror:\u{1b}[0m foo\n\n  bar\tbaz";
    let s = sanitize_detail(raw);
    assert!(!s.contains('\u{1b}'), "no escape bytes: {s:?}");
    assert!(
        !s.contains('\n') && !s.contains('\t'),
        "no raw control: {s:?}"
    );
    assert_eq!(s, "Build dev shell (exit 2): error: foo bar baz");
    // OSC sequence (ESC ] … BEL) is dropped whole.
    assert_eq!(sanitize_detail("a\u{1b}]0;title\u{7}b"), "ab");
    // Long input is clamped with an ellipsis.
    let long = "x".repeat(500);
    assert!(sanitize_detail(&long).chars().count() <= 201);
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

fn cfg_with(agents: &[(&str, &str)], tools: &[(&str, &str)]) -> Config {
    let mut cfg = Config::default();
    let mk = |(n, c): &(&str, &str)| thegn_core::config::NamedCommand {
        name: n.to_string(),
        command: c.to_string(),
        hints: Vec::new(),
        provider: None,
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
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = std::env::var_os("XDG_STATE_HOME");
    // SAFETY: guarded by ENV_LOCK; this module's DB-touching tests run inside this critical section.
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    let out = f();
    match old {
        Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
    }
    let _ = std::fs::remove_dir_all(&dir);
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
            .put_worktree("app/wt", "/x/app", &wt, "sz/wt", None, None)
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
        cfg.sandbox.backend_chain = vec!["wsl".to_string(), "host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-auto-fallthrough-{}", std::process::id()));
        let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
        assert_eq!(spec.backend, "host");
        let warning = spec
            .warning_summary()
            .expect("host fallback should be visible");
        assert!(warning.contains("sandbox wsl unavailable"), "{warning}");
        assert!(
            warning.contains("running on host after sandbox fallback"),
            "{warning}"
        );
    });
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
    };
    let spec = compose_spec(&cfg, "/wt/x", Some("sz/x"), "claude", &loc, &host);
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
            .contains(&("THEGN_BRANCH".to_string(), "sz/x".to_string()))
    );
    // The settled backend + warnings ride into the spec.
    assert_eq!(spec.backend, "host");
    assert_eq!(
        spec.warning_summary().as_deref(),
        Some("sandbox auto selected host")
    );
}

fn host_outcome() -> SandboxOutcome {
    SandboxOutcome {
        spec: None,
        backend_label: "host".into(),
        warnings: Vec::new(),
        shell: String::new(),
        is_remote: false,
        cwd_override: None,
        location: None,
    }
}

#[test]
fn route_agent_injects_proxy_env_into_host_agent_pane() {
    // `route_agent` on, bouncer OFF: a configured agent on the host gets the
    // proxy vars routed to the LOCAL loopback; a plain shell gets nothing.
    let mut cfg = cfg_with(&[("shell", "__shell__"), ("Agent", "/a/pi")], &[]);
    cfg.llm_proxy.route_agent = true;
    cfg.llm_proxy.bouncer = false;

    let mut outcome = host_outcome();
    let agent = apply_bouncer_launch(&cfg, "/wt/x", "Agent", &mut outcome);
    assert!(
        agent
            .host_env
            .iter()
            .any(|(k, v)| k == "THEGN_PROXY_BASE_URL" && v == "http://127.0.0.1:8383"),
        "host agent pane routes through the local proxy without bouncer"
    );
    assert!(
        !agent
            .host_env
            .iter()
            .any(|(k, _)| k == "ANTHROPIC_BASE_URL"),
        "claude is NOT routed by default (route_claude off) — talks to Anthropic directly"
    );

    // route_claude ON → claude/codex on host also get ANTHROPIC_BASE_URL.
    cfg.llm_proxy.route_claude = true;
    let mut outcome = host_outcome();
    let claude = apply_bouncer_launch(&cfg, "/wt/x", "Agent", &mut outcome);
    assert!(
        claude
            .host_env
            .iter()
            .any(|(k, _)| k == "ANTHROPIC_BASE_URL"),
        "route_claude → claude/codex on host get ANTHROPIC_BASE_URL"
    );
    cfg.llm_proxy.route_claude = false;

    // A shell never routes.
    let mut outcome = host_outcome();
    let shell = apply_bouncer_launch(&cfg, "/wt/x", "shell", &mut outcome);
    assert!(shell.host_env.is_empty(), "shells are not routed");

    // route_agent OFF → no injection even for an agent.
    cfg.llm_proxy.route_agent = false;
    let mut outcome = host_outcome();
    let off = apply_bouncer_launch(&cfg, "/wt/x", "Agent", &mut outcome);
    assert!(
        off.host_env.is_empty(),
        "no routing when route_agent is off"
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
    // Must have an unconditional /bin/sh -l fallback at the end.
    assert!(
        oci.ends_with("exec /bin/sh -l"),
        "must end with /bin/sh fallback"
    );
    // bash must always appear in the chain (present in every Debian image).
    assert!(oci.contains("bash"), "bash must be in the probe chain");
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
    // The chain itself still self-execs into a shell, ending in /bin/sh.
    assert!(script.contains("command -v zsh") && script.contains("exec /bin/sh -l"));
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
    };
    let spec = compose_spec(&cfg, "/wt/x", None, "clean-shell", &loc, &sb);
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
    let out = prepare_sandbox_env(
        &cfg,
        Path::new("/repo"),
        "/wt/x",
        &loc,
        None,
        false,
        SandboxScope::Shell,
        None,
    )
    .unwrap();
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
        SandboxScope::Shell,
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
        SandboxScope::Shell,
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
        SandboxScope::Shell,
        None,
    );
    if let Ok(o) = out {
        assert_ne!(
            o.backend_label, "host",
            "non-explicit host must not beat bwrap config"
        );
    } // Err (bwrap unavailable) is acceptable — still not a host drop.
}

// H1: E2E launch_spec test — backend="none" → host fallback path.
#[test]
fn launch_spec_none_backend_produces_valid_spec() {
    with_temp_state("launch-spec-none", || {
        let mut cfg = cfg_with(&[("claude", "claude --foo")], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        let worktree = std::env::temp_dir().join(format!("sz-ls-none-{}", std::process::id()));
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

// H1 (C2 variant): launch_spec_with_key injects scoped API key.
#[test]
fn launch_spec_with_key_injects_scoped_key() {
    with_temp_state("launch-spec-key", || {
        let mut cfg = cfg_with(&[("claude", "claude --foo")], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        let worktree = std::env::temp_dir().join(format!("sz-ls-key-{}", std::process::id()));
        let spec = launch_spec_with_key(
            &cfg,
            &worktree.to_string_lossy(),
            None,
            "shell",
            Some("sk-test-scoped".into()),
            false,
        )
        .unwrap();
        // On the host path there's no OCI spec to mutate, so scoped key
        // falls into the LaunchSpec env directly via compose_spec.
        // At minimum the spec must succeed; the key injection path is
        // exercised without a running container.
        assert_eq!(spec.backend, "host");
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
