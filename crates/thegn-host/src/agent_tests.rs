use super::*;

#[test]
fn sandbox_resolution_does_not_authorize_implicit_host_fallback() {
    use thegn_core::config::{SandboxBackend, SandboxConfig};
    use thegn_core::placement::Placement;

    let local = Placement::Local;
    let mut requested = SandboxConfig {
        enabled: true,
        backend: SandboxBackend::Auto,
        ..Default::default()
    };
    assert!(!host_fallback_allowed(&local, &requested, false));
    assert!(host_fallback_allowed(&local, &requested, true));

    requested.backend = SandboxBackend::None;
    assert!(host_fallback_allowed(&local, &requested, false));
    requested.enabled = false;
    requested.backend = SandboxBackend::Auto;
    assert!(host_fallback_allowed(&local, &requested, false));
}

#[test]
fn on_missing_fail_refuses_the_pane_instead_of_exiting() {
    with_temp_state("auto-host-on-missing-fail", || {
        let mut cfg = cfg_with(&[], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        cfg.sandbox.on_missing = thegn_core::config::OnMissing::Fail;
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-on-missing-{}", std::process::id()));
        // Before: `msg::die` exited the test process here.
        let err = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell")
            .expect_err("on_missing = fail refuses the pane");
        assert!(format!("{err:#}").contains("on_missing"), "{err:#}");
    });
}

#[test]
fn automatic_prewarm_keeps_remote_native_specs() {
    let spec = |remote| LaunchSpec {
        argv: vec!["ssh".into()],
        cwd: None,
        env: Vec::new(),
        backend: "host".into(),
        warnings: Vec::new(),
        degraded: false,
        remote,
    };
    // A remote native pane labels itself `host` but is not a local host shell.
    let mut remote = Ok(vec![(1, spec(true))]);
    reject_host_prewarm(&mut remote);
    assert!(remote.is_ok());
    let mut local = Ok(vec![(1, spec(false))]);
    reject_host_prewarm(&mut local);
    assert!(matches!(
        local,
        Err(crate::handlers::provision::SpecError::PrewarmSkipped)
    ));
}

#[test]
fn only_a_configured_auto_chain_naming_the_host_lands_there() {
    use thegn_core::config::{SandboxBackend, SandboxConfig};

    let mut sb = SandboxConfig {
        enabled: true,
        backend: SandboxBackend::Auto,
        ..Default::default()
    };
    // The default chain ends in `host`: landing there is configured, not a fallback.
    assert!(auto_chain_names_host(&sb));
    sb.backend_chain = vec!["none".into()];
    assert!(auto_chain_names_host(&sb));
    // A chain without the host must not inherit the implicit host tail.
    sb.backend_chain = vec!["podman-rootless".into(), "bwrap".into()];
    assert!(!auto_chain_names_host(&sb));
    // An explicit backend is a containment request, whatever the chain says.
    sb.backend_chain = vec!["host".into()];
    sb.backend = SandboxBackend::Bwrap;
    assert!(!auto_chain_names_host(&sb));
    // Disabled is the separate explicit-host policy, not this rule.
    sb.backend = SandboxBackend::Auto;
    sb.enabled = false;
    assert!(!auto_chain_names_host(&sb));
}

#[test]
fn auto_chain_without_host_halts_instead_of_opening_a_host_shell() {
    with_temp_state("auto-no-host", || {
        let mut cfg = cfg_with(&[], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        // `wsl` is a reserved kind: the chain skips it outright on every OS, so
        // no runtime is probed and only the implicit host tail remains.
        cfg.sandbox.backend_chain = vec!["wsl".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-agent-auto-no-host-{}", std::process::id()));
        let err = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell")
            .expect_err("a chain that never names the host must not open a host shell");
        assert!(
            format!("{err:#}").contains("will not silently fall back to the host"),
            "actionable halt: {err:#}"
        );
    });
}

#[test]
fn remote_native_resolution_never_uses_the_local_none_host_fallback() {
    with_temp_state("remote-none-resolution", || {
        let cfg: Config = toml::from_str(
            r#"
[sandbox]
backend = "none"

[env.ssh]
placement = "ssh"
[env.ssh.ssh]
host = "unreachable.test"
transport = "ssh"

[env.provider]
placement = "provider"
[env.provider.provider]
provider = "custom"
id = "fixture"
exec_command = ["fixture-exec", "{id}", "--"]
"#,
        )
        .unwrap();
        let loc = GitLoc::from_db("/local/worktree", None);

        // The nested `backend = none` is a valid native remote execution
        // choice. It must return a remote spec before the final no-candidate
        // refusal, without probing a live SSH/provider service.
        let ssh = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/local/worktree",
            &loc,
            None,
            false,
            Some("ssh"),
        )
        .expect("native ssh resolution does not need a host fallback");
        assert!(ssh.spec.is_some());
        assert!(ssh.is_remote);
        // `Backend::None` labels itself "host"; `is_remote` (and LaunchSpec.remote)
        // is what distinguishes it from a local host shell for prewarm.
        assert_eq!(ssh.backend_label, "host");

        // The provider fixture is likewise resolved by its injected static
        // placement outcome; no provider API or availability probe is needed.
        let provider = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/local/worktree",
            &loc,
            None,
            false,
            Some("provider"),
        )
        .expect("native provider resolution does not need a host fallback");
        assert!(provider.spec.is_some());
        assert!(provider.is_remote);
        assert_eq!(provider.backend_label, "host");

        // A disabled remote environment still has a remote placement and must
        // not turn a missing nested backend into a local host shell: it either
        // halts (a disabled sandbox resolves no remote spec, so the non-local
        // no-candidate halt fires).
        let mut disabled = cfg.clone();
        disabled.sandbox.enabled = false;
        let err = prepare_sandbox_env(
            &disabled,
            Path::new("/repo"),
            "/local/worktree",
            &loc,
            None,
            false,
            Some("ssh"),
        )
        .expect_err("a disabled remote env refuses rather than opening a local shell");
        assert!(
            err.downcast_ref::<crate::agent::SandboxHalt>().is_some(),
            "{err:#}"
        );
    });
}

#[test]
fn automatic_prewarm_rejects_host_specs_but_keeps_contained_specs() {
    let mut host = Ok(vec![(
        7,
        LaunchSpec {
            argv: vec!["fake-shell".into()],
            cwd: None,
            env: Vec::new(),
            backend: "host".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        },
    )]);
    reject_host_prewarm(&mut host);
    assert!(matches!(
        host,
        Err(crate::handlers::provision::SpecError::PrewarmSkipped)
    ));

    let mut contained = Ok(vec![(
        7,
        LaunchSpec {
            argv: vec!["fake-bwrap".into()],
            cwd: None,
            env: Vec::new(),
            backend: "bwrap".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        },
    )]);
    reject_host_prewarm(&mut contained);
    assert!(contained.is_ok(), "contained prewarm remains eligible");
}

#[test]
fn automatic_prewarm_rejects_host_reintroduced_by_remembered_agent_relaunch() {
    with_temp_state("prewarm-relaunch-host", || {
        let mut cfg = cfg_with(&[("remembered", "remembered-agent")], &[]);
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        let worktree =
            std::env::temp_dir().join(format!("tg-prewarm-relaunch-host-{}", std::process::id()));
        let wt = worktree.to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
            .unwrap();
        db.set_worktree_agent(&wt, "remembered").unwrap();
        drop(db);

        // Model the first guard's contained resolution, then the remembered
        // agent fold that can replace its first leaf with a host spec. The
        // second guard must inspect the post-relaunch batch, not only the
        // result that existed before resurrection.
        let (specs, _) = crate::handlers::prewarm::resolve_automatic_with(
            Some(7),
            || {
                Ok(vec![(
                    7,
                    LaunchSpec {
                        argv: vec!["fake-contained-shell".into()],
                        cwd: None,
                        env: Vec::new(),
                        backend: "bwrap".into(),
                        warnings: Vec::new(),
                        degraded: false,
                        remote: false,
                    },
                )])
            },
            Vec::<crate::handlers::worktree_attach::AttachTarget>::new,
            |specs, first_leaf, attach_is_empty| {
                crate::handlers::worktree_launch::apply_relaunch(
                    specs,
                    &cfg,
                    &wt,
                    first_leaf,
                    attach_is_empty,
                    false,
                );
                assert!(
                    specs.as_ref().ok().expect("resolved launch specs")[0]
                        .1
                        .argv
                        .join(" ")
                        .contains("remembered-agent"),
                    "the real prewarm batch includes the remembered-agent substitution"
                );
                assert_eq!(
                    specs.as_ref().ok().expect("resolved launch specs")[0]
                        .1
                        .backend,
                    "host"
                );
            },
        );

        assert!(matches!(
            specs,
            Err(crate::handlers::provision::SpecError::PrewarmSkipped)
        ));
    });
}

/// Positive control: a focused user launch may still choose the host backend;
/// only the automatic prewarm route applies the host rejection policy.
#[test]
fn focused_host_launch_remains_an_explicit_positive_control() {
    with_temp_state("focused-host-positive", || {
        let mut cfg = cfg_with(&[], &[]);
        cfg.sandbox.enabled = false;
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        let worktree =
            std::env::temp_dir().join(format!("tg-focused-host-positive-{}", std::process::id()));
        let spec = crate::direnv_warm::launch_spec_synced_with(
            &cfg,
            &worktree.to_string_lossy(),
            None,
            "shell",
            LaunchExtras::default(),
        )
        .expect("an explicit focused host launch remains available");
        assert_eq!(spec.backend, "host");
    });
}

#[cfg(unix)]
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test fixture: a blocking wait on a fake binary is the positive control"
)]
fn automatic_prewarm_drains_host_result_without_spawning_or_evaluating() {
    use std::os::unix::fs::PermissionsExt;

    with_temp_state("prewarm-executable-drain", || {
        let root = std::env::temp_dir().join(format!(
            "tg-prewarm-executable-drain-{}",
            std::process::id()
        ));
        let worktree = root.join("repo");
        let fake_bin = root.join("bin");
        let shell_ran = root.join("shell-ran");
        let direnv_ran = root.join("direnv-ran");
        let nix_ran = root.join("nix-ran");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&fake_bin).unwrap();
        std::fs::write(
            worktree.join(".envrc"),
            format!("printf hostile > {}\n", root.join("envrc-ran").display()),
        )
        .unwrap();
        std::fs::write(worktree.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        std::fs::write(worktree.join("flake.lock"), "locked\n").unwrap();

        let script = |path: &std::path::Path, marker: &std::path::Path| {
            std::fs::write(
                path,
                format!("#!/bin/sh\nprintf called > {}\n", marker.display()),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let fake_shell = fake_bin.join("shell");
        script(&fake_shell, &shell_ran);
        script(&fake_bin.join("direnv"), &direnv_ran);
        script(&fake_bin.join("nix"), &nix_ran);
        // Positive controls prove the same executable sentinels can fire.
        for (program, marker) in [
            (&fake_shell, &shell_ran),
            (&fake_bin.join("direnv"), &direnv_ran),
            (&fake_bin.join("nix"), &nix_ran),
        ] {
            assert!(
                std::process::Command::new(program)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(marker.exists());
            std::fs::remove_file(marker).unwrap();
        }

        let old_shell = std::env::var_os("SHELL");
        let old_path = std::env::var_os("PATH");
        let mut path = fake_bin.as_os_str().to_os_string();
        path.push(":/usr/bin:/bin");
        // SAFETY: with_temp_state holds ENV_LOCK for this whole test.
        unsafe {
            std::env::set_var("SHELL", &fake_shell);
            std::env::set_var("PATH", &path);
        }

        let mut cfg = cfg_with(&[("remembered", "placeholder")], &[]);
        cfg.agents[0].command = fake_bin.join("remembered-agent").display().to_string();
        cfg.sandbox.enabled = false;
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        cfg.sandbox.warm_direnv = thegn_core::config::WarmDirenv::Auto;
        cfg.sandbox.inject_devshell = true;
        let wt = worktree.to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open().unwrap();
        db.put_worktree("app/wt", "/x/app", &wt, "tg/wt", None, None)
            .unwrap();
        db.set_worktree_agent(&wt, "remembered").unwrap();
        drop(db);

        // Resolve through the same automatic-prewarm helper used by the real
        // worker. The fake evaluator executables and hostile files provide the
        // external evidence; this test does not manufacture a skip with a
        // separate reject call.
        let (specs, _) = crate::handlers::prewarm::resolve_automatic_with(
            Some(7),
            || {
                crate::direnv_warm::launch_spec_synced_with(
                    &cfg,
                    &wt,
                    None,
                    "shell",
                    LaunchExtras {
                        suppress_agent_record: true,
                        ..Default::default()
                    },
                )
                .map(|spec| {
                    assert_eq!(spec.backend, "host", "fixture starts with a host result");
                    vec![(7, spec)]
                })
                .map_err(crate::handlers::provision::spec_err)
            },
            Vec::<crate::handlers::worktree_attach::AttachTarget>::new,
            |specs, first_leaf, attach_is_empty| {
                crate::handlers::worktree_launch::apply_relaunch(
                    specs,
                    &cfg,
                    &wt,
                    first_leaf,
                    attach_is_empty,
                    false,
                );
            },
        );
        assert!(matches!(
            specs,
            Err(crate::handlers::provision::SpecError::PrewarmSkipped)
        ));

        let mut session = crate::session::Session {
            id: "s1".into(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "app/wt",
                crate::session::GroupKind::Branch,
                wt.clone(),
            )],
            active: 0,
        };
        session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(7);
        session.worktrees[0].tabs[0].focused_pane = 7;
        let (pane_tx, _pane_rx) = tokio::sync::mpsc::channel::<crate::pane::PaneEvent>(1024);
        let mut panes = crate::panes::Panes::new(pane_tx);
        let mut model = crate::chrome::FrameModel::default();
        let mut active_menu: Option<crate::menu::MenuOverlay> = None;
        let mut loading_state = crate::loading::track::LoadingTracker::default();
        let mut loading_remote = std::collections::HashMap::new();
        let mut materialize_inflight = std::collections::HashSet::new();
        let mut prewarm_inflight = std::collections::HashSet::from([("app/wt".into(), 0)]);
        let mut materialize_failed = std::collections::HashSet::new();
        let mut prewarm_failed = std::collections::HashSet::new();
        let mut halt_dismissed = std::collections::HashSet::new();
        let mut last_pool_reconcile = None;
        let mut center_dormant = false;
        let mut need_relayout = false;
        let mut dirty = false;
        let mut loop_perf = crate::perf::LoopPerf::new();
        let (spec_tx, mut spec_rx) = tokio::sync::mpsc::unbounded_channel();
        spec_tx
            .send(crate::handlers::provision::SpecBatch {
                group: "app/wt".into(),
                worktree: wt,
                tab: 0,
                target_leaves: vec![7],
                origin: crate::loading::SpecOrigin::Prewarm,
                specs,
                attach: Vec::new(),
            })
            .unwrap();

        crate::handlers::provision::drain_specs(
            &mut spec_rx,
            &mut crate::handlers::provision::SpecDrainCtx {
                session: &mut session,
                panes: &mut panes,
                model: &mut model,
                active_menu: &mut active_menu,
                current_config: &cfg,
                center: crate::layout::compute(160, 40, true, true).center,
                loading_state: &mut loading_state,
                loading_remote: &mut loading_remote,
                materialize_inflight: &mut materialize_inflight,
                prewarm_inflight: &mut prewarm_inflight,
                materialize_failed: &mut materialize_failed,
                prewarm_failed: &mut prewarm_failed,
                halt_dismissed: &mut halt_dismissed,
                last_pool_reconcile: &mut last_pool_reconcile,
                center_dormant: &mut center_dormant,
                need_relayout: &mut need_relayout,
                dirty: &mut dirty,
                loop_perf: &mut loop_perf,
            },
        );

        assert!(
            panes.table.is_empty(),
            "prewarm skip must not spawn a host pane"
        );
        assert!(
            prewarm_inflight.is_empty(),
            "the skipped request is settled"
        );
        assert!(prewarm_failed.is_empty(), "a benign skip is not a failure");
        assert!(!dirty, "no pane was spawned or attached");
        assert!(!root.join("envrc-ran").exists());
        assert!(!shell_ran.exists());
        assert!(!direnv_ran.exists());
        assert!(!nix_ran.exists());

        match old_shell {
            Some(value) => unsafe { std::env::set_var("SHELL", value) },
            None => unsafe { std::env::remove_var("SHELL") },
        }
        match old_path {
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[cfg(unix)]
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test fixture: a blocking wait on a fake binary is the positive control"
)]
fn removed_direnv_warm_is_inert_across_launch_seams_and_cache_leaf_shapes() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
    use std::time::SystemTime;

    with_temp_state("direnv-zero-exec", || {
        let root = std::env::temp_dir().join(format!("tg-direnv-zero-exec-{}", std::process::id()));
        let worktree = root.join("repo");
        let fake_bin = root.join("bin");
        let direnv_calls = root.join("direnv-calls");
        let nix_calls = root.join("nix-calls");
        std::fs::create_dir_all(worktree.join(".direnv")).unwrap();
        std::fs::create_dir_all(&fake_bin).unwrap();
        std::fs::write(
            worktree.join(".envrc"),
            format!("printf hostile > {}\n", root.join("envrc-ran").display()),
        )
        .unwrap();
        std::fs::write(worktree.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        std::fs::write(worktree.join("flake.lock"), "locked\n").unwrap();
        for (name, marker) in [("direnv", &direnv_calls), ("nix", &nix_calls)] {
            std::fs::write(
                fake_bin.join(name),
                format!("#!/bin/sh\nprintf called > {}\n", marker.display()),
            )
            .unwrap();
            std::fs::set_permissions(fake_bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            // Positive control: an attempted evaluator call must leave evidence.
            assert!(
                std::process::Command::new(fake_bin.join(name))
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(marker.exists());
            std::fs::remove_file(marker).unwrap();
        }

        let external = root.join("external.rc");
        let hard_target = root.join("hard-target.rc");
        let symlink_leaf = worktree.join(".direnv/symlink.rc");
        let hardlink_leaf = worktree.join(".direnv/hardlink.rc");
        let fifo_leaf = worktree.join(".direnv/fifo.rc");
        let directory_leaf = worktree.join(".direnv/directory.rc");
        std::fs::write(&external, b"external-bytes\n").unwrap();
        std::fs::write(&hard_target, b"hard-bytes\n").unwrap();
        symlink(&external, &symlink_leaf).unwrap();
        std::fs::hard_link(&hard_target, &hardlink_leaf).unwrap();
        let fifo_name = std::ffi::CString::new(fifo_leaf.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        std::fs::create_dir(&directory_leaf).unwrap();
        // Force stale cache targets so the removed blessing path would touch
        // them if accidentally restored. Fresh fixture mtimes would hide it.
        for target in [&external, &hard_target] {
            std::fs::File::options()
                .write(true)
                .open(target)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60))
                .unwrap();
        }
        let external_bytes = std::fs::read(&external).unwrap();
        let hard_bytes = std::fs::read(&hard_target).unwrap();
        let external_mtime: SystemTime = std::fs::metadata(&external).unwrap().modified().unwrap();
        let hard_mtime: SystemTime = std::fs::metadata(&hard_target).unwrap().modified().unwrap();

        let old_path = std::env::var_os("PATH");
        let mut path = fake_bin.as_os_str().to_os_string();
        path.push(":/usr/bin:/bin");
        // SAFETY: with_temp_state holds ENV_LOCK for this whole test.
        unsafe { std::env::set_var("PATH", &path) };
        let mut cfg = Config::default();
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        cfg.sandbox.inject_devshell = false;
        let wt = worktree.to_string_lossy().into_owned();
        for mode in [
            thegn_core::config::WarmDirenv::Auto,
            thegn_core::config::WarmDirenv::AllowedOnly,
            thegn_core::config::WarmDirenv::Off,
        ] {
            cfg.sandbox.warm_direnv = mode;
            assert_eq!(thegn_core::direnv::warm_now_plan(mode), None);
            crate::agent::launch_spec_full(
                &cfg,
                &wt,
                None,
                "shell",
                false,
                LaunchExtras::default(),
            )
            .unwrap();
            crate::agent::launch_spec_center_with(
                &cfg,
                &wt,
                None,
                "shell",
                LaunchExtras::default(),
            )
            .unwrap();
            crate::direnv_warm::launch_spec_synced_with(
                &cfg,
                &wt,
                None,
                "shell",
                LaunchExtras::default(),
            )
            .unwrap();
            crate::agent::prewarm_spec(&cfg, &wt).unwrap();
        }
        match old_path {
            Some(path) => unsafe { std::env::set_var("PATH", path) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert!(!root.join("envrc-ran").exists());
        assert!(!direnv_calls.exists());
        assert!(!nix_calls.exists());
        assert_eq!(std::fs::read(&external).unwrap(), external_bytes);
        assert_eq!(std::fs::read(&hard_target).unwrap(), hard_bytes);
        assert_eq!(std::fs::read_link(&symlink_leaf).unwrap(), external);
        assert_eq!(std::fs::read(&hardlink_leaf).unwrap(), hard_bytes);
        assert!(
            std::fs::symlink_metadata(&fifo_leaf)
                .unwrap()
                .file_type()
                .is_fifo()
        );
        assert!(directory_leaf.is_dir());
        assert_eq!(
            std::fs::metadata(&external).unwrap().modified().unwrap(),
            external_mtime
        );
        assert_eq!(
            std::fs::metadata(&hard_target).unwrap().modified().unwrap(),
            hard_mtime
        );
        std::fs::remove_dir_all(&root).unwrap();
    });
}

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
    assert!(joined.contains("-F /dev/null"), "{joined}");
    assert!(joined.contains("BatchMode=yes"), "{joined}");
    assert!(joined.contains("IdentitiesOnly=yes"), "{joined}");
    assert!(joined.contains("ControlMaster=no"), "{joined}");
    assert!(joined.contains("ControlPath=none"), "{joined}");
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
    assert!(s.contains("mkdir -p \"$HOME/.ssh\" || exit 73"));
    assert!(s.contains("chmod 700 \"$HOME/.ssh\" || exit 73"));
    assert!(s.contains("chmod 600 \"$HOME/.ssh/authorized_keys\" || exit 73"));
    assert!(
        !s.ends_with("true"),
        "a final true must not mask setup failure"
    );
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

fn sandbox_with_invalid_volume() -> thegn_core::sandbox::SandboxSpec {
    thegn_core::sandbox::SandboxSpec {
        backend: thegn_core::sandbox::Backend::Podman,
        placement: thegn_core::placement::Placement::Local,
        image: Some("img:latest".into()),
        worktree: PathBuf::from("/wt/x"),
        mounts: Vec::new(),
        env: Vec::new(),
        env_overrides: std::collections::HashMap::new(),
        env_block: Vec::new(),
        network: thegn_core::config::Network::Nat,
        network_allow: Vec::new(),
        network_block: Vec::new(),
        read_only_root: false,
        no_new_privileges: false,
        pids_limit: None,
        drop_capabilities: Vec::new(),
        add_capabilities: Vec::new(),
        file_access: thegn_core::config::FileAccess::Worktree,
        ports: Vec::new(),
        gpu: None,
        limits: thegn_core::sandbox::SandboxLimits::default(),
        volumes: vec![("/tmp/state".into(), "/mnt/state".into())],
        compose: None,
        build: None,
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: "tg-wt".into(),
        vpn: None,
        oci_host: None,
        oci_runtime: None,
        daemon_persistent: false,
    }
}

#[test]
fn compose_spec_propagates_volume_refusal_without_host_fallback() {
    let cfg = Config::default();
    let loc = GitLoc::from_db("/wt/x", None);
    let outcome = SandboxOutcome {
        spec: Some(sandbox_with_invalid_volume()),
        backend_label: "podman".into(),
        warnings: Vec::new(),
        shell: String::new(),
        is_remote: false,
        cwd_override: None,
        location: None,
        degraded_from_provider: false,
        route_ssh_target: None,
    };
    let error = compose_spec(
        &cfg,
        "/wt/x",
        None,
        "shell",
        &loc,
        &outcome,
        LaunchExtras::default(),
    )
    .expect_err("invalid volume must prevent LaunchSpec construction");
    assert!(error.chain().any(|source| {
        source
            .downcast_ref::<thegn_core::sandbox::VolumeAdmissionError>()
            .is_some()
    }));
    assert!(!error.to_string().contains("/tmp/state"));
}

#[test]
fn final_composed_volume_gate_blocks_launch_effect_sentinel() {
    // This exercises the same production gate used after Ready/remote spec
    // composition. An invalid source must stop before any effect represented by
    // the sentinel callback (VPN, ensure, OCI argv, or host fallback).
    let mut spec = sandbox_with_invalid_volume();
    let mut effects = 0;
    let result = admit_final_sandbox_spec_then(&mut spec, |_| {
        effects += 1;
        Ok(())
    });
    assert!(result.is_err(), "invalid final spec must be terminal");
    assert_eq!(effects, 0, "no launch effect may follow refusal");
}

#[test]
fn configured_volume_refusal_precedes_agent_host_fallback() {
    with_temp_state("configured-volume-refusal", || {
        let mut cfg = Config::default();
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        cfg.sandbox.backend_chain = vec!["missing-runtime".into()];
        cfg.sandbox
            .volumes
            .insert("/tmp/state".into(), "/mnt/state".into());
        let loc = GitLoc::from_db("/wt/x", None);
        let error = prepare_sandbox_env(&cfg, Path::new("/repo"), "/wt/x", &loc, None, false, None)
            .expect_err("configured invalid volume must not fall through to host");
        assert!(error.to_string().contains("configured volume admission"));
        assert!(!error.to_string().contains("/tmp/state"));
    });
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
        route_ssh_target: None,
    };
    let spec = compose_spec(
        &cfg,
        "/wt/x",
        Some("tg/x"),
        "claude",
        &loc,
        &host,
        LaunchExtras::default(),
    )
    .expect("valid volume names");
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

#[cfg(unix)]
#[test]
fn invalidated_devcontainer_exec_refuses_before_invoking_a_host_shell() {
    use std::os::unix::fs::PermissionsExt;

    with_temp_state("devcontainer-refusal", || {
        let root = std::env::temp_dir().join(format!(
            "tg-devcontainer-refusal-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config_path = root.join("devcontainer.json");
        std::fs::write(&config_path, br#"{"image":"trusted"}"#).unwrap();
        let sentinel = root.join("host-shell-ran");
        let fake_shell = root.join("fake-shell");
        std::fs::write(
            &fake_shell,
            format!("#!/bin/sh\nprintf invoked > {}\n", sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_shell = std::env::var_os("SHELL");
        // SAFETY: with_temp_state holds ENV_LOCK for this whole test.
        unsafe {
            std::env::set_var("SHELL", fake_shell.to_str().expect("fake shell path"));
        }
        let worktree = root.to_string_lossy().into_owned();
        crate::devcontainer_provider::install_failing_test_session(&worktree, &config_path)
            .unwrap();

        let cfg = Config::default();
        let loc = GitLoc::from_db(&worktree, None);
        let outcome = SandboxOutcome {
            spec: None,
            backend_label: "devcontainer".into(),
            warnings: Vec::new(),
            shell: String::new(),
            is_remote: false,
            cwd_override: None,
            location: None,
            degraded_from_provider: false,
            route_ssh_target: None,
        };
        let error = compose_spec(
            &cfg,
            &worktree,
            None,
            "shell",
            &loc,
            &outcome,
            LaunchExtras::default(),
        )
        .expect_err("a failed provider exec must refuse the launch");
        assert!(
            error.downcast_ref::<DevcontainerLaunchRefused>().is_some(),
            "typed refusal: {error:#}"
        );
        assert!(
            !sentinel.exists(),
            "the rejected target must not be replaced by a host shell"
        );
        crate::devcontainer_provider::remove_test_session(&worktree);
        let missing = compose_spec(
            &cfg,
            &worktree,
            None,
            "shell",
            &loc,
            &outcome,
            LaunchExtras::default(),
        )
        .expect_err("a vanished provider session must not become a host launch");
        assert!(
            missing
                .downcast_ref::<DevcontainerLaunchRefused>()
                .is_some()
        );
        assert!(!sentinel.exists());
        match old_shell {
            Some(shell) => unsafe { std::env::set_var("SHELL", shell) },
            None => unsafe { std::env::remove_var("SHELL") },
        }
        std::fs::remove_dir_all(&root).unwrap();
    });
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
        route_ssh_target: None,
    };
    let spec = compose_spec(
        &cfg,
        "/wt/x",
        None,
        "clean-shell",
        &loc,
        &sb,
        LaunchExtras::default(),
    )
    .expect("valid volume names");
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
    // `none` is an intentional host decision, so `on_missing = fail` must not
    // turn the post-resolution notice into a process-fatal diagnostic.
    cfg.sandbox.on_missing = thegn_core::config::OnMissing::Fail;
    cfg.sandbox
        .volumes
        .insert("/tmp/ignored".into(), "/mnt/ignored".into());
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
    assert!(out.warnings.is_empty());
}

#[test]
fn disabled_sandbox_auto_backend_skips_host_fallback_notice() {
    let mut cfg = Config::default();
    cfg.sandbox.enabled = false;
    cfg.sandbox.on_missing = thegn_core::config::OnMissing::Fail;
    let loc = GitLoc::from_db("/wt/x", None);
    let out = prepare_sandbox_env(&cfg, Path::new("/repo"), "/wt/x", &loc, None, false, None)
        .expect("disabled sandbox is an intentional host decision");
    assert!(out.spec.is_none());
    assert_eq!(out.backend_label, "host");
    assert!(out.warnings.is_empty());
}

#[test]
fn final_host_fallback_honors_fail_closed_floor_without_a_resolved_candidate() {
    with_temp_state("floor-auto-host", || {
        let mut cfg = Config::default();
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Auto;
        // No candidate reaches the pre-ensure floor check; the chain lands on
        // the host directly. This used to bypass fail-closed admission entirely.
        cfg.sandbox.backend_chain = vec!["host".to_string()];
        cfg.sandbox.isolation_floor = thegn_core::config::IsolationFloor::SharedKernel;
        cfg.sandbox.on_floor_miss = thegn_core::config::OnFloorMiss::Fail;
        let loc = GitLoc::from_db("/wt/x", None);
        let error = prepare_sandbox_env(&cfg, Path::new("/repo"), "/wt/x", &loc, None, false, None)
            .unwrap_err();
        assert!(error.to_string().contains("isolation floor"), "{error}");
        assert!(error.to_string().contains("host-process"), "{error}");
    });
}

#[test]
fn explicit_host_does_not_override_fail_closed_floor() {
    with_temp_state("floor-explicit-host", || {
        let mut cfg = Config::default();
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::Bwrap;
        cfg.sandbox.isolation_floor = thegn_core::config::IsolationFloor::SharedKernel;
        cfg.sandbox.on_floor_miss = thegn_core::config::OnFloorMiss::Fail;
        let loc = GitLoc::from_db("/wt/x", None);
        let error = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/wt/x",
            &loc,
            Some("host"),
            true,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("isolation floor"), "{error}");
    });
}

#[test]
fn final_host_floor_degrade_reports_the_missed_boundary() {
    with_temp_state("floor-degraded-host", || {
        let mut cfg = Config::default();
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        cfg.sandbox.isolation_floor = thegn_core::config::IsolationFloor::SharedKernel;
        cfg.sandbox.on_floor_miss = thegn_core::config::OnFloorMiss::Degrade;
        let loc = GitLoc::from_db("/wt/x", None);
        let outcome =
            prepare_sandbox_env(&cfg, Path::new("/repo"), "/wt/x", &loc, None, false, None)
                .unwrap();
        assert!(outcome.spec.is_none());
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("isolation floor") && w.contains("host-process"))
        );
    });
}

#[test]
fn bare_remote_ssh_honors_fail_closed_floor() {
    with_temp_state("floor-remote-host", || {
        let mut cfg = Config::default();
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        cfg.sandbox.isolation_floor = thegn_core::config::IsolationFloor::SharedKernel;
        cfg.sandbox.on_floor_miss = thegn_core::config::OnFloorMiss::Fail;
        let target = SshTarget::plain("example.invalid".into(), 22, false);
        let location = GitLoc::remote_db_string_for(&target, "/remote/worktree");
        let loc = GitLoc::from_db("/host/worktree", Some(&location));
        let error = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/host/worktree",
            &loc,
            None,
            false,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("isolation floor"), "{error}");
    });
}

#[test]
fn prepare_remote_ssh_carries_exact_target_for_route_credential() {
    let mut cfg = Config::default();
    cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
    let target = SshTarget {
        host: "alice@build.example".into(),
        port: 2222,
        forward_agent: false,
        ssh_config: Some("/host/ssh-config".into()),
        jump_host: Some("bastion".into()),
        identity: Some("/host/key".into()),
        extra_args: vec!["-o".into(), "IdentitiesOnly=yes".into()],
    };
    let location = GitLoc::remote_db_string_for(&target, "/remote/worktree");
    let loc = GitLoc::from_db("/host/worktree", Some(&location));
    let out = prepare_sandbox_env(
        &cfg,
        Path::new("/repo"),
        "/host/worktree",
        &loc,
        None,
        false,
        None,
    )
    .unwrap();
    assert_eq!(out.route_ssh_target, Some(target));
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
    // Any failover mode ⇒ Err(SandboxHalt); only an explicit host policy ⇒ Ok,
    // with `degraded_from_provider` set so the notification/status/sidebar fire.
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

        // failover = auto is not permission to bypass a requested env (THE-418):
        // the dropped selection still halts rather than opening a host shell.
        let mut cfg = Config::default();
        cfg.sandbox.failover = thegn_core::config::FailoverMode::Auto;
        let err = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/wt/x",
            &loc,
            None,
            false,
            Some("ghost"),
        )
        .expect_err("auto failover no longer degrades a dropped selection");
        assert!(err.downcast_ref::<crate::agent::SandboxHalt>().is_some());

        // An explicit local host policy is a deliberate host decision: degrade,
        // but LOUDLY (degraded flag set) so the notification fires.
        let mut cfg = Config::default();
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        let out = prepare_sandbox_env(
            &cfg,
            Path::new("/repo"),
            "/wt/x",
            &loc,
            None,
            false,
            Some("ghost"),
        )
        .expect("an explicit host policy degrades rather than halting");
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
        remote: false,
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

#[test]
fn credential_gate_refuses_agents_only_for_live_ambiguity() {
    use thegn_core::store::WorkspaceStore;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_memory().unwrap();
    let checkout = |name: &str| {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.join(".git")).unwrap();
        path.to_string_lossy().into_owned()
    };
    let mut cfg = cfg_with(&[("codex", "codex")], &[]);
    assert!(thegn_core::account::provider_for(&cfg, "codex").is_some());
    cfg.workspace
        .entry("foo".into())
        .or_default()
        .accounts
        .insert("codex".into(), "work".into());
    // A stale (deleted) registration holds `foo`; the real repo is `foo-2`.
    let gone = dir.path().join("gone/foo").to_string_lossy().into_owned();
    db.slug_for_repo(&gone, "foo").unwrap();
    let real = checkout("code/foo");
    assert_eq!(db.slug_for_repo(&real, "foo").unwrap(), "foo-2");
    credential_overlay_gate(&cfg, &db, "foo-2", "codex").expect("stale row must not block");
    // A second LIVE checkout named `foo`: agents are refused, shells are not.
    let other = checkout("other/foo");
    db.slug_for_repo(&other, "foo").unwrap();
    let error = credential_overlay_gate(&cfg, &db, "foo-2", "codex").unwrap_err();
    assert!(
        format!("{error:#}").contains("Rename or delete"),
        "{error:#}"
    );
    credential_overlay_gate(&cfg, &db, "foo-2", "shell").expect("a shell always launches");
}

/// THE-440: a permissioned launch through the real daemon seam (`command_for`
/// → `launch_spec_full`) never reads or writes the repository tree. The grant
/// rides the process argv as the harness's command-scoped `--settings` layer,
/// so repository-controlled redirects (a final `settings.local.json` symlink
/// to a missing outside target, a symlinked `.claude` directory, a FIFO leaf
/// that would block any read) have nothing to act on; `git status` is
/// byte-identical; concurrent launches with different stage policies each
/// carry only their own grant; and a shell re-parse of the command yields the
/// exact JSON, token for token.
#[cfg(unix)]
#[test]
fn permissioned_launch_is_command_scoped_and_never_touches_the_repository() {
    use crate::daemon::agent_open::command_for;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};

    with_temp_state("the440-perm-seam", || {
        let root = std::env::temp_dir().join(format!("tg-the440-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root); // best-effort: test cleanup: stale scratch from a prior run
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("claude-dir")).unwrap();

        let git = |dir: &Path, args: &[&str]| -> Vec<u8> {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            out.stdout
        };
        let repo = |name: &str, setup: &dyn Fn(&Path)| -> PathBuf {
            let wt = root.join(name);
            std::fs::create_dir_all(&wt).unwrap();
            git(&wt, &["init", "-q", "-b", "main"]);
            std::fs::write(wt.join("README"), "x\n").unwrap();
            setup(&wt);
            git(&wt, &["add", "-A"]);
            git(&wt, &["commit", "-q", "-m", "fixture"]);
            wt
        };
        // (a) tracked final-leaf symlink to a MISSING outside target.
        let missing = outside.join("missing.json");
        let wt_leaf = repo("leaf-symlink", &|wt| {
            std::fs::create_dir_all(wt.join(".claude")).unwrap();
            symlink(&missing, wt.join(".claude/settings.local.json")).unwrap();
        });
        // (b) tracked `.claude` directory symlink to an outside directory.
        let wt_dir = repo("dir-symlink", &|wt| {
            symlink(outside.join("claude-dir"), wt.join(".claude")).unwrap();
        });
        // (c) a FIFO leaf (untracked): any read by the launch would block.
        let wt_fifo = repo("fifo-leaf", &|wt| {
            std::fs::create_dir_all(wt.join(".claude")).unwrap();
            std::fs::write(wt.join(".claude/keep"), "").unwrap();
        });
        let fifo = wt_fifo.join(".claude/settings.local.json");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        // (d) the user's own settings: deny rules, hooks, unknown keys.
        let user_bytes = b"{\n  \"permissions\": { \"allow\": [\"Old\"], \"deny\": [\"Bash(rm:*)\"] },\n  \"hooks\": { \"Stop\": [] },\n  \"x-unknown\": 1\n}\n";
        let wt_user = repo("user-settings", &|wt| {
            std::fs::create_dir_all(wt.join(".claude")).unwrap();
            std::fs::write(wt.join(".claude/settings.local.json"), user_bytes).unwrap();
        });
        let worktrees = [&wt_leaf, &wt_dir, &wt_fifo, &wt_user];

        let mut cfg = cfg_with(&[("worker", "claude")], &[]);
        cfg.agents[0].permissions = vec!["Read".into()];
        cfg.sandbox.backend = thegn_core::config::SandboxBackend::None;
        cfg.sandbox.inject_devshell = false;
        for (name, perms, harness) in [
            ("code", vec!["Edit", "Bash(cargo test:*)"], None),
            ("review", vec!["Read", "Bash(echo 'a' \"$(id)\")"], None),
            ("fanout", vec!["Read"], Some("pi")),
        ] {
            cfg.pipeline
                .stages
                .push(thegn_core::config_pipeline::PipelineStage {
                    name: name.into(),
                    agent: "worker".into(),
                    harness: harness.map(str::to_string),
                    permissions: perms.into_iter().map(str::to_string).collect(),
                    prompt: "x".into(),
                    ..Default::default()
                });
        }
        let grant = |stage: &str| -> String {
            let allow = &cfg.pipeline.stage(stage).unwrap().permissions;
            serde_json::json!({ "permissions": { "allow": allow } }).to_string()
        };

        let status = |wt: &Path| git(wt, &["status", "--porcelain=v1", "-z", "--ignored"]);
        let before: Vec<Vec<u8>> = worktrees.iter().map(|wt| status(wt)).collect();
        let outside_listing = |p: &Path| -> Vec<String> {
            let mut v: Vec<String> = walkdir_names(p);
            v.sort();
            v
        };
        let outside_before = outside_listing(&outside);

        let launch = |wt: &Path, stage: &str| -> LaunchSpec {
            let cmd = command_for(&cfg, "worker", "do it", true, None, false, Some(stage))
                .expect("claude grants command-scoped");
            launch_spec_full(
                &cfg,
                &wt.to_string_lossy(),
                None,
                "worker",
                true,
                LaunchExtras {
                    cmd_override: Some(&cmd),
                    prompt: Some("do it"),
                    stage: Some(stage),
                    ..Default::default()
                },
            )
            .expect("launch resolves")
        };

        // Concurrent launches with different policies on every worktree.
        std::thread::scope(|s| {
            let handles: Vec<_> = worktrees
                .iter()
                .flat_map(|wt| {
                    ["code", "review"].map(|stage| {
                        let launch = &launch;
                        s.spawn(move || (stage, launch(wt, stage)))
                    })
                })
                .collect();
            for h in handles {
                let (stage, spec) = h.join().unwrap();
                let other = if stage == "code" { "review" } else { "code" };
                let argv = spec.argv.join("\n");
                assert!(
                    argv.contains(&thegn_core::util::sh_quote(&grant(stage))),
                    "{stage}: its own grant rides the argv: {argv}"
                );
                assert!(
                    !argv.contains(&grant(other)),
                    "{stage}: never another launch's grant"
                );
            }
        });

        // Resume and continue carry the same grant; an unattestable harness
        // (the pi stage) refuses rather than dropping it.
        for (resume, cont) in [(Some("abc-123"), false), (None, true)] {
            let cmd = command_for(&cfg, "worker", "", true, resume, cont, Some("code")).unwrap();
            assert!(
                cmd.ends_with(&format!(
                    " --settings {}",
                    thegn_core::util::sh_quote(&grant("code"))
                )),
                "{cmd}"
            );
        }
        let err = command_for(&cfg, "worker", "do it", true, None, false, Some("fanout"))
            .expect_err("pi cannot take a command-scoped grant");
        assert!(err.to_string().contains("permission policy hold"), "{err}");

        // A real shell parses the command back to the exact argv.
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let args_out = root.join("args");
        std::fs::write(
            bin.join("claude"),
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > \"$ARGS_OUT\"\n",
        )
        .unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let cmd = command_for(&cfg, "worker", "do it", true, None, false, Some("review")).unwrap();
        let ok = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&wt_user)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("ARGS_OUT", &args_out)
            .status()
            .unwrap();
        assert!(ok.success());
        let raw = std::fs::read(&args_out).unwrap();
        let tokens: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|t| !t.is_empty())
            .map(|t| String::from_utf8(t.to_vec()).unwrap())
            .collect();
        assert_eq!(
            tokens,
            vec![
                "-p".to_string(),
                "do it".into(),
                "--permission-mode".into(),
                "acceptEdits".into(),
                "--settings".into(),
                grant("review"),
            ]
        );

        // Nothing moved: the repository trees, the outside targets, the FIFO,
        // and the user's own settings bytes.
        let after: Vec<Vec<u8>> = worktrees.iter().map(|wt| status(wt)).collect();
        assert_eq!(before, after, "git status must be byte-identical");
        assert!(!missing.exists(), "a dangling redirect was never followed");
        assert_eq!(outside_listing(&outside), outside_before);
        assert!(
            std::fs::symlink_metadata(&fifo)
                .unwrap()
                .file_type()
                .is_fifo()
        );
        assert_eq!(
            std::fs::read(wt_user.join(".claude/settings.local.json")).unwrap(),
            user_bytes
        );
        std::fs::remove_dir_all(&root).unwrap();
    });
}

/// Every path under `dir` (relative), not following symlinks.
#[cfg(unix)]
fn walkdir_names(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            out.push(p.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
            if std::fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p);
            }
        }
    }
    out
}
