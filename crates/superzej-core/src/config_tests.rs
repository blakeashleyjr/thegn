use super::*;

#[test]
fn lifecycle_defaults_are_budget_safe() {
    let l = LifecycleConfig::default();
    assert!(
        l.enabled,
        "policy on by default (defaults only reduce cost)"
    );
    assert_eq!(l.max_warm, 2);
    assert_eq!(l.idle_ttl_secs, 300);
    assert_eq!(l.eager, EagerScope::ActiveWorktreePlusNew);
    assert!(l.keep_active_warm && l.keep_busy_warm && l.serve_cached_glyphs);
    assert_eq!(l.cost_ceiling_per_hour, 0.0, "no ceiling by default");
    assert_eq!(l.pool.size, 0, "pool disabled by default");
}

#[test]
fn nix_parallel_clamps_and_gates_on_zero() {
    let mut pc = EnvProviderConfig::default();
    assert_eq!(pc.nix_parallel(), None, "0 ⇒ leave nix defaults");
    assert!(pc.is_default(), "speedup fields default to inert");
    pc.nix_parallel_downloads = 100;
    assert_eq!(pc.nix_parallel(), Some(100));
    pc.nix_parallel_downloads = 9999;
    assert_eq!(pc.nix_parallel(), Some(256), "clamped to 256");
    assert!(!pc.is_default());
}

#[test]
fn eager_scope_and_nix_installer_parse() {
    assert_eq!(
        EagerScope::from_str_validated("focus"),
        Ok(EagerScope::ActiveWorktreePlusNew)
    );
    assert_eq!(
        EagerScope::from_str_validated("workspace"),
        Ok(EagerScope::ActiveWorkspace)
    );
    assert_eq!(EagerScope::from_str_validated("off"), Ok(EagerScope::Off));
    assert!(EagerScope::from_str_validated("bogus").is_err());
    assert_eq!(
        NixInstaller::from_str_validated("ds"),
        Ok(NixInstaller::Determinate)
    );
    assert_eq!(NixInstaller::default(), NixInstaller::Official);
}

#[test]
fn short_hash_is_stable_and_distinct() {
    // Stable across calls (the property the sandbox name lifecycle relies on).
    assert_eq!(util::short_hash("/a/b/c", 6), util::short_hash("/a/b/c", 6));
    assert_eq!(util::short_hash("/a/b/c", 6).len(), 6);
    assert_ne!(util::short_hash("/a/b/c", 6), util::short_hash("/a/b/d", 6));
    // base36 charset only.
    assert!(
        util::short_hash("/x/y/z", 6)
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    );
}

#[test]
fn provider_exec_mode_parses_and_defaults_to_auto() {
    assert_eq!(ProviderExecMode::default(), ProviderExecMode::Auto);
    assert_eq!(
        ProviderExecMode::from_str_validated("api"),
        Ok(ProviderExecMode::Api)
    );
    assert_eq!(
        ProviderExecMode::from_str_validated("CLI"),
        Ok(ProviderExecMode::Cli)
    );
    assert_eq!(ProviderExecMode::Auto.as_str(), "auto");
    assert!(ProviderExecMode::from_str_validated("nope").is_err());
    // A fresh provider block is "default" (so it round-trips as absent) and
    // its exec mode is Auto.
    let pc = EnvProviderConfig::default();
    assert!(pc.is_default());
    assert_eq!(pc.exec, ProviderExecMode::Auto);
}

#[test]
fn app_tab_config_defaults_to_work_first_and_default() {
    let cfg = Config::default();
    assert_eq!(cfg.apps.default_tab, "work");
    assert_eq!(cfg.apps.effective_tab_order(), vec!["work"]);
}

#[test]
fn app_tab_config_honors_file_env_and_cli_order() {
    let mut env = MapEnv::default();
    // Only `work` is a built-in id today; every other requested id is
    // filtered out.
    env.0.insert(
        "SUPERZEJ_APPS_TAB_ORDER".into(),
        "comms,work,dashboard".into(),
    );
    env.0
        .insert("SUPERZEJ_APPS_DEFAULT_TAB".into(), "work".into());
    let flags = vec!["apps.default_tab=work".to_string()];
    let cfg = Config::load_layered(&env, &flags, None);

    assert_eq!(cfg.apps.default_tab, "work");
    assert_eq!(cfg.apps.effective_tab_order(), vec!["work"]);
}

#[test]
fn disk_config_defaults_and_env_override() {
    let cfg = Config::default();
    assert!(cfg.disk.show_sizes);
    assert_eq!(cfg.disk.warn_threshold_gb, 100);
    assert_eq!(cfg.disk.scan_interval_secs, 45);
    assert!(cfg.disk.auto_clean_on_merge);
    assert!(!cfg.disk.clean_on_pr_closed);
    assert!(!cfg.disk.sccache);
    assert!(cfg.disk.sccache_dir.is_empty());
    assert!(cfg.disk.shared_target_dir.is_empty());

    let mut env = MapEnv::default();
    env.0.insert("SUPERZEJ_DISK_SCCACHE".into(), "true".into());
    env.0
        .insert("SUPERZEJ_DISK_WARN_THRESHOLD_GB".into(), "250".into());
    env.0.insert(
        "SUPERZEJ_DISK_SHARED_TARGET_DIR".into(),
        "/tmp/shared".into(),
    );
    let cfg = Config::load_layered(&env, &[], None);
    assert!(cfg.disk.sccache);
    assert_eq!(cfg.disk.warn_threshold_gb, 250);
    assert_eq!(cfg.disk.shared_target_dir, "/tmp/shared");
}

#[test]
fn try_load_layered_handles_overrides_and_invalid_overrides() {
    let env = MapEnv::default();
    let cli_overrides = vec![
        "theme.accent=#abcdef".to_string(),
        "invalid.path=123".to_string(),
        "sandbox.enabled=false".to_string(),
        "sandbox.remote.host=user@box".to_string(),
    ];

    let cfg = Config::try_load_layered(&env, &cli_overrides, None).unwrap();
    assert_eq!(cfg.theme.accent, "#abcdef");
    assert!(!cfg.sandbox.enabled);
    assert_eq!(cfg.sandbox.remote.host, "user@box");
}

#[test]
fn override_str_parses_types_correctly_and_handles_bad_paths() {
    let mut cfg = Config::default();
    // Number
    assert!(Config::apply_override_str(&mut cfg, "repo_scan_depth", "99").is_ok());
    assert_eq!(cfg.repo_scan_depth, 99);
    // Bool
    assert!(Config::apply_override_str(&mut cfg, "sandbox.enabled", "false").is_ok());
    assert!(!cfg.sandbox.enabled);
    // String
    assert!(Config::apply_override_str(&mut cfg, "theme.accent", "#123456").is_ok());
    assert_eq!(cfg.theme.accent, "#123456");
    // Deep error: parent is not an object
    assert!(Config::apply_override_str(&mut cfg, "repo_scan_depth.invalid", "value").is_err());
    // Deep error: parent is missing/null
    assert!(Config::apply_override_str(&mut cfg, "does.not.exist", "value").is_err());
    // Type error: setting a number field to a string that doesn't parse to a number
    assert!(Config::apply_override_str(&mut cfg, "repo_scan_depth", "not_a_number").is_err());

    // Edge cases
    assert!(Config::apply_override_str(&mut cfg, "theme", "value").is_err());
    assert!(Config::apply_override_str(&mut cfg, "drawer.height", "\"30%\"").is_ok());

    // Null test
    assert!(Config::apply_override_str(&mut cfg, "sandbox.remote", "value").is_err());
}

#[test]
fn plugin_manifest_config_projection_parses() {
    let cfg: Config = toml::from_str(
        r#"
[[plugins]]
id = "todoist"
name = "Todoist"
version = "1.0.0"
api = "0.1.0"
capabilities = ["surface:statusbar"]

[[plugins.contributions]]
id = "todoist.count"
extension_point = "StatusBarSegment"
label = "Todoist"
surface = "todoist.status"
"#,
    )
    .unwrap();

    assert_eq!(cfg.plugins.len(), 1);
    assert_eq!(cfg.plugins[0].id.as_str(), "todoist");
    assert_eq!(
        cfg.plugins[0].contributions[0].extension_point,
        crate::plugin_api::ExtensionPoint::StatusBarSegment
    );
}

#[test]
fn worktree_templates_parse_with_defaults() {
    let cfg: Config = toml::from_str(
        r#"
[[worktree_templates]]
name = "rust-feature"
base = "main"
branch_prefix = "feat/"
sandbox = "podman"
agent = "claude"
pins = ["logs", "test-watch"]
commands = ["nvim", "", "cargo watch -x test"]

[[worktree_templates]]
name = "minimal"
"#,
    )
    .unwrap();
    assert_eq!(cfg.worktree_templates.len(), 2);
    let t = &cfg.worktree_templates[0];
    assert_eq!(t.name, "rust-feature");
    assert_eq!(t.base.as_deref(), Some("main"));
    assert_eq!(t.branch_prefix.as_deref(), Some("feat/"));
    assert_eq!(t.sandbox.as_deref(), Some("podman"));
    assert_eq!(t.agent.as_deref(), Some("claude"));
    assert_eq!(t.pins, vec!["logs", "test-watch"]);
    assert_eq!(t.commands.len(), 3);
    // A bare template defaults every optional field.
    let m = &cfg.worktree_templates[1];
    assert_eq!(m.name, "minimal");
    assert!(m.base.is_none() && m.agent.is_none() && m.layout.is_none());
    assert!(m.pins.is_empty() && m.commands.is_empty());
    // Default config has no templates.
    assert!(Config::default().worktree_templates.is_empty());
}

#[test]
fn monitor_defaults() {
    let m = MonitorConfig::default();
    assert_eq!(m.system, "btm");
    assert_eq!(m.gpu, "nvtop");
}

#[test]
fn stats_defaults() {
    let s = StatsConfig::default();
    assert_eq!(s.refresh_secs, 2.0);
    // Nerd Font glyphs by default; overridable to plain text. All must be
    // single-width PUA glyphs (U+E000–U+F8FF) so the icon sits flush with
    // its value — plane-15 MDI glyphs (U+F0000+) double-advance and leave a
    // gap. See StatsConfig::default.
    for (name, icon) in [
        ("cpu", &s.cpu_icon),
        ("mem", &s.mem_icon),
        ("net", &s.net_icon),
        ("gpu", &s.gpu_icon),
        ("temp", &s.temp_icon),
        ("swap", &s.swap_icon),
        ("freq", &s.freq_icon),
        ("load", &s.load_icon),
        ("uptime", &s.uptime_icon),
        ("disk", &s.disk_icon),
        ("battery", &s.battery_icon),
        ("battery_charging", &s.battery_charging_icon),
    ] {
        let cp = icon.chars().next().unwrap() as u32;
        assert!(
            (0xE000..=0xF8FF).contains(&cp),
            "{name} icon U+{cp:04X} must be single-width PUA (U+E000–U+F8FF)"
        );
    }
    assert_eq!(s.cpu_icon, "\u{f4bc}");
    assert_eq!(s.mem_icon, "\u{efc5}");
    assert_eq!(s.net_icon, "\u{f1eb}"); // nf-fa-wifi
    assert_eq!(s.gpu_icon, "\u{f2db}"); // nf-fa-microchip
    assert_eq!(s.battery_icon, "\u{f240}"); // nf-fa-battery_full
    // nf-fa-bolt — lightning bolt shown while charging.
    assert_eq!(s.battery_charging_icon, "\u{f0e7}");
    assert_eq!(s.battery_warn, 25);
    assert_eq!(s.refresh_rates, vec![1.0, 2.0, 5.0, 10.0]);
}

#[test]
fn monitor_command_maps_kinds() {
    let cfg = Config::default();
    assert_eq!(cfg.monitor_command("cpu"), Some("btm"));
    assert_eq!(cfg.monitor_command("mem"), Some("btm"));
    assert_eq!(cfg.monitor_command("gpu"), Some("nvtop"));
    assert_eq!(cfg.monitor_command("disk"), None);
    assert_eq!(cfg.monitor_command(""), None);
}

#[test]
fn monitor_command_honors_overrides() {
    let cfg = Config {
        monitor: MonitorConfig {
            system: "htop".into(),
            gpu: "nvitop".into(),
        },
        ..Config::default()
    };
    assert_eq!(cfg.monitor_command("cpu"), Some("htop"));
    assert_eq!(cfg.monitor_command("gpu"), Some("nvitop"));
}

#[test]
fn missing_monitor_table_uses_defaults() {
    // A config.toml without a [monitor] table parses with serde defaults.
    let cfg: Config = toml::from_str("base_branch = \"main\"").unwrap();
    assert_eq!(cfg.monitor.system, "btm");
    assert_eq!(cfg.monitor.gpu, "nvtop");
}

#[test]
fn parses_monitor_table() {
    let cfg: Config = toml::from_str("[monitor]\nsystem = \"htop\"\ngpu = \"nvtop\"\n").unwrap();
    assert_eq!(cfg.monitor.system, "htop");
    assert_eq!(cfg.monitor.gpu, "nvtop");
}

#[test]
fn partial_monitor_table_keeps_serde_defaults() {
    // Only one key set — the other falls back to its default.
    let cfg: Config = toml::from_str("[monitor]\ngpu = \"nvitop\"\n").unwrap();
    assert_eq!(cfg.monitor.system, "btm");
    assert_eq!(cfg.monitor.gpu, "nvitop");
}

#[test]
fn parse_hex_rgb_accepts_3_and_6_digit_and_rejects_junk() {
    assert_eq!(parse_hex_rgb("#76eede").as_deref(), Some("118;238;222"));
    assert_eq!(parse_hex_rgb("#fff").as_deref(), Some("255;255;255"));
    assert_eq!(parse_hex_rgb("#000").as_deref(), Some("0;0;0"));
    assert_eq!(parse_hex_rgb("76eede"), None); // requires a leading '#'
    assert_eq!(parse_hex_rgb("#12g456"), None);
    assert_eq!(parse_hex_rgb("#1234"), None);
    assert_eq!(parse_hex_rgb(""), None);
}

#[test]
fn accent_helpers_fall_back_to_teal_on_bad_hex() {
    let good = Config {
        theme: ThemeConfig {
            accent: "#FFffFF".into(),
            ..ThemeConfig::default()
        },
        ..Config::default()
    };
    assert_eq!(good.accent_rgb(), "255;255;255");
    assert_eq!(good.accent_hex(), "#ffffff"); // normalized to lowercase
    let bad = Config {
        theme: ThemeConfig {
            accent: "not-a-color".into(),
            ..ThemeConfig::default()
        },
        ..Config::default()
    };
    assert_eq!(bad.accent_hex(), "#6ee7d8");
    assert_eq!(bad.accent_rgb(), crate::theme::HUE_TEAL);
}

#[test]
fn palette_defaults_match_builtins() {
    let p = Config::default().palette();
    assert_eq!(p, crate::theme::Palette::default());
    assert_eq!(p.focus, crate::theme::HUE_TEAL);
    assert_eq!(p.border, crate::theme::P_GHOST);
    assert_eq!(p.accent, crate::theme::HUE_TEAL);
}

#[test]
fn legacy_presets_come_back_fully_extended() {
    let cfg = Config::default();
    for name in crate::theme::PRESETS {
        let p = cfg.palette_with_preset(name);
        assert!(!p.ghost2.is_empty(), "{name}: ghost2");
        assert!(!p.shadow_bg.is_empty(), "{name}: shadow_bg");
        assert!(!p.hues.orange.is_empty(), "{name}: hues");
        assert!(p.heat.iter().all(|h| !h.is_empty()), "{name}: heat");
    }
}

#[test]
fn activity_dot_colors_resolve_and_honor_overrides() {
    let cfg = Config::default();
    for name in crate::theme::PRESETS {
        let p = cfg.palette_with_preset(name);
        // Default: active borrows the text tone, waiting borrows red.
        assert_eq!(p.activity_active, p.text, "{name}: activity_active");
        assert_eq!(p.activity_waiting, p.hues.red, "{name}: activity_waiting");
    }
    // Explicit `[theme.colors]` overrides win over the derived defaults.
    let mut cfg = Config::default();
    cfg.theme.colors.activity_active = Some("#010203".into());
    cfg.theme.colors.activity_waiting = Some("#0a0b0c".into());
    let p = cfg.palette();
    assert_eq!(p.activity_active, "1;2;3");
    assert_eq!(p.activity_waiting, "10;11;12");
}

#[test]
fn derived_tokens_follow_overridden_bases_and_hue_overrides_apply() {
    let mut cfg = Config::default();
    cfg.theme.preset = "storm".into();
    cfg.theme.colors.ghost = Some("#808080".into());
    cfg.theme.hues.red = Some("#ff0000".into());
    let p = cfg.palette();
    // ghost2 derives from the *overridden* ghost, not storm's.
    assert_eq!(
        p.ghost2,
        crate::theme::blend_over("128;128;128", &p.bg0, 0.62)
    );
    assert_eq!(p.hues.red, "255;0;0");
    // Explicit extension override beats derivation.
    cfg.theme.colors.ghost2 = Some("#010203".into());
    assert_eq!(cfg.palette().ghost2, "1;2;3");
}

#[test]
fn old_default_accent_still_reads_as_uncustomized() {
    // A config that pinned the pre-prism default accent keeps preset
    // accents when cycling (treated as "not customized").
    let mut cfg = Config::default();
    cfg.theme.accent = "#76eede".into();
    cfg.theme.focus_border = "#9bd1ff".into();
    let p = cfg.palette_with_preset("ember");
    assert_eq!(p.accent, "255;122;89"); // ember's own accent survives
    assert_eq!(p.focus, "255;176;102"); // ember's own focus survives
}

#[test]
fn palette_applies_overrides_and_skips_bad_hex() {
    let mut cfg = Config::default();
    cfg.theme.focus_border = "#102030".into();
    cfg.theme.colors.bg0 = Some("#000000".into());
    cfg.theme.colors.border = Some("#fff".into()); // short form
    cfg.theme.colors.text = Some("nope".into()); // invalid -> default
    let p = cfg.palette();
    assert_eq!(p.focus, "16;32;48");
    assert_eq!(p.bg0, "0;0;0");
    assert_eq!(p.border, "255;255;255");
    assert_eq!(p.text, crate::theme::P_TEXT);
}

#[test]
fn theme_keys_via_get_set_and_env() {
    let mut cfg = Config::default();
    assert!(Config::apply_override_str(&mut cfg, "theme.focus_border", "#abcdef").is_ok());
    assert!(Config::apply_override_str(&mut cfg, "theme.colors.bg1", "#111111").is_ok());
    assert_eq!(cfg.get_dotted("theme.focus_border").unwrap(), "#abcdef");
    assert_eq!(cfg.get_dotted("theme.colors.bg1").unwrap(), "#111111");
    assert_eq!(cfg.get_dotted("theme.colors.bg0").unwrap(), "");
    assert_eq!(cfg.get_dotted("theme.colors.bogus"), None);

    let env = map_env(&[
        ("SUPERZEJ_THEME_FOCUS_BORDER", "#010203"),
        ("SUPERZEJ_THEME_BORDER", "#040506"),
    ]);
    let mut base = Config::default();
    env_overlay(&env).apply(&mut base);
    assert_eq!(base.theme.focus_border, "#010203");
    assert_eq!(base.theme.colors.border.as_deref(), Some("#040506"));
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sz-cfg-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
    MapEnv(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

#[test]
fn sandbox_profile_defaults_and_env_overlay() {
    // Safe-by-default: the worktree shell is hardened, the embedded agent
    // gets its own sealed container.
    let c = SandboxConfig::default();
    assert_eq!(c.profile, SandboxProfile::Hardened);
    assert_eq!(c.agent_profile, SandboxProfile::Sealed);

    let o = env_overlay(&map_env(&[
        ("SUPERZEJ_SANDBOX_PROFILE", "open"),
        ("SUPERZEJ_SANDBOX_AGENT_PROFILE", "hardened"),
    ]));
    assert_eq!(o.sandbox.profile, Some(SandboxProfile::Open));
    assert_eq!(o.sandbox.agent_profile, Some(SandboxProfile::Hardened));

    // Overlay precedence: a present key overrides the global default.
    let mut base = SandboxConfig::default();
    o.sandbox.apply(&mut base);
    assert_eq!(base.profile, SandboxProfile::Open);
    assert_eq!(base.agent_profile, SandboxProfile::Hardened);
}

// The same overlay expressed in each format must produce identical results,
// and only the present keys override the global defaults.
#[test]
fn repo_overlay_all_three_formats_agree() {
    let cfg = Config::default();
    let cases = [
        (
            "toml",
            ".superzej.toml",
            "[sandbox]\nimage = \"img:1\"\ninit_script = \"echo hi\"\n[sandbox.remote]\nhost = \"user@box\"\n",
        ),
        (
            "yaml",
            ".superzej.yaml",
            "sandbox:\n  image: img:1\n  init_script: echo hi\n  remote:\n    host: user@box\n",
        ),
        (
            "json",
            ".superzej.json",
            "{\"sandbox\":{\"image\":\"img:1\",\"init_script\":\"echo hi\",\"remote\":{\"host\":\"user@box\"}}}",
        ),
    ];
    for (tag, file, body) in cases {
        let dir = tmpdir(tag);
        std::fs::write(dir.join(file), body).unwrap();
        // All three formats must parse to the *same clamped* result: image +
        // init_script are TOFU-gated (surfaced as pending, not applied) and
        // remote is forbidden (denied). Format agreement is the point.
        let r = cfg.repo_sandbox_resolved(&dir, &crate::config_resolve::Approvals::deny_all());
        assert_eq!(r.sandbox.image, "", "{tag}: image gated, keeps default");
        assert_eq!(r.sandbox.init_script, "", "{tag}: init_script gated");
        assert_eq!(r.sandbox.remote.host, "", "{tag}: remote host denied");
        assert!(
            r.pending.iter().any(|p| p.key == "sandbox.image"),
            "{tag}: image request surfaced"
        );
        assert!(
            r.pending.iter().any(|p| p.key == "sandbox.init_script"),
            "{tag}: init_script request surfaced"
        );
        assert!(
            r.events.iter().any(|e| e.key == "sandbox.remote"),
            "{tag}: remote denial surfaced"
        );
        assert!(r.sandbox.enabled, "{tag}: enabled keeps default");
        assert_eq!(r.sandbox.backend, SandboxBackend::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn no_repo_file_yields_global() {
    let cfg = Config::default();
    let dir = tmpdir("none");
    let sb = cfg.repo_sandbox(&dir);
    assert_eq!(sb.image, ""); // global default (host-toolchain)
    assert!(!sb.remote.is_remote());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workspace_sandbox_mounts_extend_global() {
    let dir = tmpdir("ws-binds");
    // Same base slug repo_sandbox derives (slugify(repo_name); no DB).
    let base = util::slugify(&crate::repo::repo_name(&dir));
    let slug = if base.is_empty() {
        "repo".to_string()
    } else {
        base
    };
    let mut cfg = Config::default();
    cfg.sandbox.mounts = vec!["/srv/global".into()];
    cfg.workspace.insert(
        slug,
        WorkspaceConfig {
            sandbox_mounts: vec!["~/datasets:ro".into()],
            ..Default::default()
        },
    );
    let sb = cfg.repo_sandbox(&dir);
    // Global mount survives, workspace mount is appended and tilde-expanded.
    assert!(sb.mounts.iter().any(|m| m == "/srv/global"));
    assert!(
        sb.mounts
            .iter()
            .any(|m| m.ends_with("/datasets:ro") && !m.starts_with('~')),
        "workspace mount appended + tilde-expanded: {:?}",
        sb.mounts
    );
    // A workspace with no entry for this slug adds nothing.
    let mut other = Config::default();
    other.workspace.insert(
        "some-other-repo".into(),
        WorkspaceConfig {
            sandbox_mounts: vec!["/should/not/appear".into()],
            ..Default::default()
        },
    );
    let sb2 = other.repo_sandbox(&dir);
    assert!(!sb2.mounts.iter().any(|m| m == "/should/not/appear"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drawer_defaults() {
    let d = DrawerConfig::default();
    assert_eq!(d.command, "");
    assert_eq!(d.config_home, ""); // empty = private default
    assert_eq!(d.height, "35%");
    assert_eq!(d.width, "full");
    assert!(!d.image_previews);
    assert!(d.git_status);
    assert!(d.contain);
    assert_eq!(d.memory_max, "2G");
    assert_eq!(d.memory_swap_max, "512M");
    assert_eq!(d.cpu_quota, "200%");
    assert_eq!(d.pool_limit, 1);
    assert!(d.prewarm);
}

#[test]
fn config_without_drawer_section_uses_defaults() {
    let cfg: Config = toml::from_str("base_branch = \"main\"").unwrap();
    assert_eq!(cfg.drawer.height, "35%");
    assert_eq!(cfg.drawer.width, "full");
    assert_eq!(cfg.drawer.command, "");
    assert!(!cfg.drawer.image_previews);
    assert!(cfg.drawer.contain);
    assert_eq!(cfg.drawer.memory_max, "2G");
    assert_eq!(cfg.drawer.memory_swap_max, "512M");
    assert_eq!(cfg.drawer.cpu_quota, "200%");
    assert_eq!(cfg.drawer.pool_limit, 1);
    assert!(cfg.drawer.prewarm);
}

#[test]
fn drawer_section_overrides_parse() {
    let cfg: Config = toml::from_str(
            "[drawer]\ncommand = \"ranger\"\nconfig_home = \"system\"\nheight = \"50%\"\nwidth = \"center\"\nimage_previews = true\ncontain = false\nmemory_max = \"4G\"\nmemory_swap_max = \"0\"\ncpu_quota = \"50%\"\npool_limit = 0\nprewarm = false\n",
        )
        .unwrap();
    assert_eq!(cfg.drawer.command, "ranger");
    assert_eq!(cfg.drawer.config_home, "system");
    assert_eq!(cfg.drawer.height, "50%");
    assert_eq!(cfg.drawer.width, "center");
    assert!(cfg.drawer.image_previews);
    assert!(!cfg.drawer.contain);
    assert_eq!(cfg.drawer.memory_max, "4G");
    assert_eq!(cfg.drawer.memory_swap_max, "0");
    assert_eq!(cfg.drawer.cpu_quota, "50%");
    assert_eq!(cfg.drawer.pool_limit, 0);
    assert!(!cfg.drawer.prewarm);
}

#[test]
fn drawer_partial_section_keeps_other_defaults() {
    // Only height set; the rest fall back to defaults via #[serde(default)].
    let cfg: Config = toml::from_str("[drawer]\nheight = \"20%\"\n").unwrap();
    assert_eq!(cfg.drawer.height, "20%");
    assert_eq!(cfg.drawer.width, "full");
    assert_eq!(cfg.drawer.command, "");
    assert!(!cfg.drawer.image_previews);
    assert!(cfg.drawer.contain);
    assert_eq!(cfg.drawer.pool_limit, 1);
    assert!(cfg.drawer.prewarm);
}

#[test]
fn git_section_and_custom_commands_parse() {
    let cfg: Config = toml::from_str(
        r#"
[git]
override_gpg = true

[[git_commands]]
key = "p"
context = "branches"
command = "git push {{.SelectedBranch.Name | quote}}"
output = "terminal"
description = "push selected branch"
prompts = [{ type = "input", title = "Remote", key = "Remote" }]

[[git_commands]]
key = "n"
command = "git notes add {{.SelectedCommit.Sha}}"
"#,
    )
    .unwrap();
    assert!(cfg.git.override_gpg);
    assert_eq!(cfg.git_commands.len(), 2);
    let c = &cfg.git_commands[0];
    assert_eq!(c.key, "p");
    assert_eq!(c.context, "branches");
    assert_eq!(c.output, GitCmdOutput::Terminal);
    assert_eq!(c.description.as_deref(), Some("push selected branch"));
    assert_eq!(c.prompts.len(), 1);
    assert_eq!(c.prompts[0].key, "Remote");
    assert_eq!(c.prompts[0].kind, "input");
    assert_eq!(c.prompts[0].title.as_deref(), Some("Remote"));
    // Defaults: context global, popup output, no prompts.
    let c = &cfg.git_commands[1];
    assert_eq!(c.context, "global");
    assert_eq!(c.output, GitCmdOutput::Popup);
    assert!(c.prompts.is_empty());

    // Absent section → defaults.
    let cfg: Config = toml::from_str("").unwrap();
    assert!(!cfg.git.override_gpg);
    assert!(cfg.git_commands.is_empty());
}

#[test]
fn panel_sections_parse_and_default_empty() {
    let cfg: Config =
        toml::from_str("[panel]\nsections = [\"pr\", \"changes\", \"telemetry\"]\n").unwrap();
    assert_eq!(cfg.panel.sections, vec!["pr", "changes", "telemetry"]);
    // Absent table → empty list (the host shows every section).
    let cfg: Config = toml::from_str("").unwrap();
    assert!(cfg.panel.sections.is_empty());
}

#[test]
fn panel_collapse_on_escape_defaults_true_and_parses() {
    // Default (both the Rust `Default` and the absent-table serde path).
    assert!(PanelConfig::default().collapse_on_escape);
    let cfg: Config = toml::from_str("").unwrap();
    assert!(cfg.panel.collapse_on_escape);
    // Explicit opt-out.
    let cfg: Config = toml::from_str("[panel]\ncollapse_on_escape = false\n").unwrap();
    assert!(!cfg.panel.collapse_on_escape);
}

#[test]
fn config_parses_mode_specific_keybinds() {
    let cfg: Config = toml::from_str(
            "[keybinds]\nnew-worktree = \"Alt w\"\n[keybinds.vim_normal]\nfocus-down = \"j\"\n[keybinds.emacs]\nquit = \"Ctrl x Ctrl c\"\n",
        )
        .unwrap();
    assert_eq!(
        cfg.keybinds.get("new-worktree").map(String::as_str),
        Some("Alt w")
    );
    assert_eq!(
        cfg.keybinds
            .vim_normal
            .get("focus-down")
            .map(String::as_str),
        Some("j")
    );
    assert_eq!(
        cfg.keybinds.emacs.get("quit").map(String::as_str),
        Some("Ctrl x Ctrl c")
    );
}

#[test]
fn keybind_config_serializes_nested_mode_tables() {
    let mut cfg = Config::default();
    cfg.keybinds.insert("new-worktree".into(), "Ctrl w".into());
    cfg.keybinds
        .vim_normal
        .insert("focus-down".into(), "j".into());
    let s = toml::to_string_pretty(&cfg).unwrap();
    assert!(s.contains("[keybinds]"));
    assert!(s.contains("new-worktree = \"Ctrl w\""));
    assert!(s.contains("[keybinds.vim_normal]"));
    assert!(s.contains("focus-down = \"j\""));
}

#[test]
fn config_parses_profiles_and_active_profile() {
    let cfg: Config = toml::from_str(
            "profile = \"vim\"\n[profiles.vim]\ndefault_mode = \"vim-normal\"\n[profiles.vim.keybinds]\nfocus-down = \"j\"\n",
        )
        .unwrap();
    let p = cfg.active_profile().expect("active profile resolves");
    assert_eq!(p.default_mode, "vim-normal");
    assert_eq!(p.keybinds.get("focus-down").map(String::as_str), Some("j"));
}

#[test]
fn unknown_profile_has_no_active_profile() {
    let cfg: Config = toml::from_str("profile = \"nope\"\n").unwrap();
    assert!(cfg.active_profile().is_none());
}

#[test]
fn effective_keybinds_layers_profile_then_global() {
    let cfg: Config = toml::from_str(
            "profile = \"vim\"\n[keybinds]\nfocus-down = \"Ctrl j\"\n[profiles.vim.keybinds]\nfocus-down = \"j\"\n",
        )
        .unwrap();
    let layers = cfg.effective_keybinds(None, None);
    // profile layer first (lowest precedence), then global.
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].get("focus-down").map(String::as_str), Some("j"));
    assert_eq!(
        layers[1].get("focus-down").map(String::as_str),
        Some("Ctrl j")
    );
}

#[test]
fn effective_keybinds_adds_central_workspace_layer_for_slug() {
    let cfg: Config = toml::from_str(
            "[keybinds]\nfocus-down = \"Ctrl j\"\n[workspace.myrepo.keybinds]\nfocus-down = \"Alt j\"\n",
        )
        .unwrap();
    let none = cfg.effective_keybinds(None, None);
    assert_eq!(none.len(), 1); // global only
    let with = cfg.effective_keybinds(None, Some("myrepo"));
    assert_eq!(with.len(), 2);
    assert_eq!(with[1].get("focus-down").map(String::as_str), Some("Alt j"));
}

// G2: Profile sandbox overlay applied by repo_sandbox().
#[test]
fn profile_sandbox_overlay_applies_network_block() {
    let cfg: Config = toml::from_str(
        "profile = \"work\"\n\
             [profiles.work.sandbox]\n\
             network_block = [\"social.example.com\"]\n",
    )
    .unwrap();
    // repo_sandbox on any path should now inherit the profile block-list.
    let sb = cfg.repo_sandbox(std::path::Path::new("/nonexistent"));
    assert!(
        sb.network_block.contains(&"social.example.com".to_string()),
        "profile network_block should flow into repo_sandbox: {:?}",
        sb.network_block
    );
}

#[test]
fn profile_sandbox_overlay_does_not_apply_when_inactive() {
    let cfg: Config = toml::from_str(
        "profile = \"\"\n\
             [profiles.work.sandbox]\n\
             network_block = [\"social.example.com\"]\n",
    )
    .unwrap();
    let sb = cfg.repo_sandbox(std::path::Path::new("/nonexistent"));
    assert!(
        sb.network_block.is_empty(),
        "inactive profile must not inject block list: {:?}",
        sb.network_block
    );
}

#[test]
fn repo_overlay_keybinds_are_the_most_specific_layer() {
    let dir = tmpdir("repo-kb");
    std::fs::write(
        dir.join(".superzej.toml"),
        "[keybinds]\nfocus-down = \"Alt n\"\n",
    )
    .unwrap();
    let cfg: Config = toml::from_str(
            "[keybinds]\nfocus-down = \"Ctrl j\"\n[workspace.myrepo.keybinds]\nfocus-down = \"Alt j\"\n",
        )
        .unwrap();
    let layers = cfg.effective_keybinds(Some(&dir), Some("myrepo"));
    // global, central-workspace, repo-root overlay (last = highest precedence).
    assert_eq!(layers.len(), 3);
    assert_eq!(
        layers.last().unwrap().get("focus-down").map(String::as_str),
        Some("Alt n")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn profile_selectable_via_env() {
    let env = MapEnv(BTreeMap::from([(
        "SUPERZEJ_PROFILE".to_string(),
        "emacs".to_string(),
    )]));
    let cfg = Config::load_layered(&env, &[], None);
    assert_eq!(cfg.profile, "emacs");
}

// defaults < file < env < flag, for a scalar and a validated enum.
#[test]
fn precedence_default_file_env_flag() {
    let dir = tmpdir("prec");
    let file = dir.join("config.toml");
    std::fs::write(&file, "branch_prefix = \"file/\"\npicker = \"gum\"\n").unwrap();

    // file only
    let c = Config::load_layered(&MapEnv::default(), &[], Some(file.clone()));
    assert_eq!(c.branch_prefix, "file/");
    assert_eq!(c.picker, Picker::Gum);

    // env overrides file
    let env = map_env(&[
        ("SUPERZEJ_BRANCH_PREFIX", "env/"),
        ("SUPERZEJ_PICKER", "fzf"),
    ]);
    let c = Config::load_layered(&env, &[], Some(file.clone()));
    assert_eq!(c.branch_prefix, "env/");
    assert_eq!(c.picker, Picker::Fzf);

    let flags = vec![
        "branch_prefix=flag/".to_string(),
        "picker=select".to_string(),
    ];
    let c = Config::load_layered(&env, &flags, Some(file));
    assert_eq!(c.picker, Picker::Select);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_enum_warns_and_defaults() {
    // A junk picker in the file deserializes to the default, never errors.
    let c: Config = toml::from_str("picker = \"nope\"\n").unwrap();
    assert_eq!(c.picker, Picker::Auto);
    // strict validate flags it
    let errs = validate_str("picker = \"nope\"\n");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("picker"));
}

#[test]
fn pin_location_defaults_to_tab() {
    let cfg: Config = toml::from_str("[[pins]]\nname = 'x'\ncommand = 'echo x'\n").unwrap();
    assert_eq!(cfg.pins[0].location, PinLocation::Tab);
}

#[test]
fn pin_location_parses_layout() {
    let cfg: Config =
        toml::from_str("[[pins]]\nname = 'x'\ncommand = 'echo x'\nlocation = 'layout'\n").unwrap();
    assert_eq!(cfg.pins[0].location, PinLocation::Layout);
    assert_eq!(PinLocation::Layout.as_str(), "layout");
}

#[test]
fn pin_location_bad_value_defaults_but_validate_flags_it() {
    let body = "[[pins]]\nname = 'x'\ncommand = 'echo x'\nlocation = 'bogus'\n";
    let cfg: Config = toml::from_str(body).unwrap();
    assert_eq!(cfg.pins[0].location, PinLocation::Tab);
    let errs = validate_str(body);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("pins[0].location"), "{errs:?}");
}

#[test]
fn pin_location_parses_strip_and_float_with_aliases() {
    let strip: Config =
        toml::from_str("[[pins]]\nname='x'\ncommand='c'\nlocation='top-strip'\n").unwrap();
    assert_eq!(strip.pins[0].location, PinLocation::Strip);
    assert_eq!(PinLocation::Strip.as_str(), "strip");
    let float: Config =
        toml::from_str("[[pins]]\nname='x'\ncommand='c'\nlocation='scratch'\n").unwrap();
    assert_eq!(float.pins[0].location, PinLocation::Float);
    assert_eq!(PinLocation::Float.as_str(), "float");
}

#[test]
fn pin_extended_fields_parse() {
    let body = "[[pins]]\nname='logs'\ncommand='journalctl'\nargs=['-f']\n\
                    label='syslog'\nratio=2.5\n[pins.env]\nRUST_LOG='info'\n";
    let cfg: Config = toml::from_str(body).unwrap();
    let p = &cfg.pins[0];
    assert_eq!(p.args, vec!["-f"]);
    assert_eq!(p.display_label(), "syslog");
    assert_eq!(p.strip_weight(), 2.5);
    assert_eq!(p.env.get("RUST_LOG").map(String::as_str), Some("info"));
}

#[test]
fn pin_helpers_fall_back_sensibly() {
    let cfg: Config = toml::from_str("[[pins]]\nname='bare'\ncommand='c'\n").unwrap();
    let p = &cfg.pins[0];
    // No label → name; no/zero ratio → 1.0.
    assert_eq!(p.display_label(), "bare");
    assert_eq!(p.strip_weight(), 1.0);
    let mut neg = p.clone();
    neg.ratio = Some(-3.0);
    assert_eq!(neg.strip_weight(), 1.0);
}

#[test]
fn strip_config_defaults_and_clamps() {
    let def = StripConfig::default();
    assert_eq!(def.ratio, 0.2);
    assert!(def.visible);
    let lo = StripConfig {
        ratio: 0.001,
        visible: true,
    };
    assert_eq!(lo.clamped_ratio(), 0.05);
    let hi = StripConfig {
        ratio: 5.0,
        visible: false,
    };
    assert_eq!(hi.clamped_ratio(), 0.9);
}

#[test]
fn pins_for_workspace_filters_by_scope() {
    let body = "[[pins]]\nname='g'\ncommand='c'\nscope='global'\n\
                    [[pins]]\nname='w'\ncommand='c'\nscope='workspace'\nworkspace='repoA'\n";
    let cfg: Config = toml::from_str(body).unwrap();
    let a: Vec<_> = cfg
        .pins_for_workspace(Some("repoA"))
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(a, vec!["g", "w"]);
    let b: Vec<_> = cfg
        .pins_for_workspace(Some("repoB"))
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(b, vec!["g"]);
}

#[test]
fn deprecated_sz_pr_ttl_still_read() {
    let env = map_env(&[("SZ_PR_TTL", "7")]);
    let o = env_overlay(&env);
    assert_eq!(o.pr_ttl_secs, Some(7));
    // canonical wins when both set
    let env = map_env(&[("SZ_PR_TTL", "7"), ("SUPERZEJ_PR_TTL", "9")]);
    assert_eq!(env_overlay(&env).pr_ttl_secs, Some(9));
}

#[test]
fn enum_roundtrip() {
    for (s, p) in [
        ("auto", Picker::Auto),
        ("gum", Picker::Gum),
        ("fzf", Picker::Fzf),
        ("select", Picker::Select),
    ] {
        assert_eq!(Picker::from_str_validated(s).unwrap(), p);
        assert_eq!(p.as_str(), s);
    }
    assert!(Picker::from_str_validated("bogus").is_err());
    // aliases
    assert_eq!(
        SandboxBackend::from_str_validated("bubblewrap").unwrap(),
        SandboxBackend::Bwrap
    );
    assert_eq!(
        SandboxBackend::from_str_validated("host").unwrap(),
        SandboxBackend::None
    );
}

#[test]
fn get_dotted_reads_values() {
    let c = Config::default();
    assert_eq!(c.get_dotted("picker").as_deref(), Some("auto"));
    assert_eq!(c.get_dotted("pr.ttl_secs").as_deref(), Some("30"));
    assert_eq!(c.get_dotted("sandbox.backend").as_deref(), Some("auto"));
    assert!(c.get_dotted("nope.nope").is_none());
}

#[test]
fn effective_config_serializes_to_toml() {
    // `config show` round-trips the effective config back to parseable TOML.
    let c = Config::default();
    let s = toml::to_string_pretty(&c).expect("serialize");
    let back: Config = toml::from_str(&s).expect("reparse");
    assert_eq!(back.picker, c.picker);
    assert_eq!(back.sandbox.backend, c.sandbox.backend);
}

#[test]
fn metrics_config_defaults_and_toml_parse() {
    let default = MetricsConfig::default();
    assert_eq!(default.interval_secs, 5.0);
    assert_eq!(default.timeout_ms, 500);
    assert_eq!(default.max_body_bytes, 1_048_576);
    assert!(default.targets.is_empty());

    let cfg: Config = toml::from_str(
        r#"
            [metrics]
            interval_secs = 2.5
            timeout_ms = 250
            max_body_bytes = 4096

            [[metrics.targets]]
            name = "model-proxy"
            url = "http://127.0.0.1:9091/metrics"
            metrics = ["http_requests_total", "process_resident_memory_bytes"]
            labels = { instance = "local" }
            "#,
    )
    .unwrap();
    assert_eq!(cfg.metrics.interval_secs, 2.5);
    assert_eq!(cfg.metrics.timeout_ms, 250);
    assert_eq!(cfg.metrics.max_body_bytes, 4096);
    assert_eq!(cfg.metrics.targets.len(), 1);
    let target = &cfg.metrics.targets[0];
    assert_eq!(target.name, "model-proxy");
    assert_eq!(target.url, "http://127.0.0.1:9091/metrics");
    assert_eq!(target.metrics[0], "http_requests_total");
    assert_eq!(
        target.labels.get("instance").map(String::as_str),
        Some("local")
    );
}

#[test]
fn metrics_env_overlay_clamps_runtime_bounds() {
    let env = map_env(&[
        ("SUPERZEJ_METRICS_INTERVAL_SECS", "0.2"),
        ("SUPERZEJ_METRICS_TIMEOUT_MS", "10"),
        ("SUPERZEJ_METRICS_MAX_BODY_BYTES", "0"),
    ]);
    let c = Config::load_layered(&env, &[], None);
    assert_eq!(c.metrics.interval_secs, 1.0);
    assert_eq!(c.metrics.timeout_ms, 100);
    assert_eq!(c.metrics.max_body_bytes, 1);
}

// Exercise every env knob (and the canonical/deprecated/bad-value paths) so
// the layering is covered, not just spot-checked.
#[test]
fn env_overlay_covers_every_knob() {
    let env = map_env(&[
        ("SUPERZEJ_WORKTREES_DIR", "/wt"),
        ("SUPERZEJ_WORKSPACES_DIR", "/ws"),
        ("SUPERZEJ_BASE_BRANCH", "develop"),
        ("SUPERZEJ_BRANCH_PREFIX", "x/"),
        ("SUPERZEJ_PICKER", "fzf"),
        ("SUPERZEJ_WORKTREE_MODE", "in_repo"),
        ("SUPERZEJ_NAME_SCHEME", "numbered"),
        ("SUPERZEJ_AUTO_REMOVE_WORKTREE", "yes"),
        ("SUPERZEJ_REPO_SCAN_DEPTH", "9"),
        ("SUPERZEJ_PROFILE", "vim"),
        ("SUPERZEJ_THEME_ACCENT", "#abcdef"),
        ("SUPERZEJ_PR_TTL", "11"),
        ("SUPERZEJ_WATCH_PR_INTERVAL", "13"),
        ("SUPERZEJ_METRICS_INTERVAL_SECS", "3.5"),
        ("SUPERZEJ_METRICS_TIMEOUT_MS", "750"),
        ("SUPERZEJ_METRICS_MAX_BODY_BYTES", "2048"),
        ("SUPERZEJ_LOG_LEVEL", "debug"),
        ("SUPERZEJ_LOG_FILE", "true"),
        ("SUPERZEJ_LOG_DIR", "/logs"),
        ("SUPERZEJ_LOG_ROTATION_SIZE_MB", "8"),
        ("SUPERZEJ_LOG_MAX_FILES", "4"),
        ("SUPERZEJ_LOG_FORMAT", "json"),
        ("SUPERZEJ_SANDBOX_BACKEND", "docker"),
        ("SUPERZEJ_SANDBOX_NETWORK", "host"),
        ("SUPERZEJ_SANDBOX_IMAGE", "img:9"),
        ("SUPERZEJ_SANDBOX_ON_MISSING", "fail"),
        ("SUPERZEJ_SANDBOX_ENABLED", "off"),
        ("SUPERZEJ_SANDBOX_REMOTE_HOST", "user@box"),
    ]);
    let c = Config::load_layered(&env, &[], None);
    assert_eq!(c.worktrees_dir, "/wt");
    assert_eq!(c.workspaces_dir, "/ws");
    assert_eq!(c.base_branch, "develop");
    assert_eq!(c.branch_prefix, "x/");
    assert_eq!(c.picker, Picker::Fzf);
    assert_eq!(c.worktree_mode, WorktreeMode::InRepo);
    assert_eq!(c.name_scheme, NameScheme::Numbered);
    assert!(c.auto_remove_worktree);
    assert_eq!(c.repo_scan_depth, 9);
    assert_eq!(c.profile, "vim");
    assert_eq!(c.theme.accent, "#abcdef");
    assert_eq!(c.pr.ttl_secs, 11);
    assert_eq!(c.watch.pr_interval_secs, 13);
    assert_eq!(c.metrics.interval_secs, 3.5);
    assert_eq!(c.metrics.timeout_ms, 750);
    assert_eq!(c.metrics.max_body_bytes, 2048);
    assert_eq!(c.log.level, LogLevel::Debug);
    assert!(c.log.file);
    assert_eq!(c.log.dir, "/logs");
    assert_eq!(c.log.rotation_size_mb, 8);
    assert_eq!(c.log.max_files, 4);
    assert_eq!(c.log.format, LogFormat::Json);
    assert_eq!(c.sandbox.backend, SandboxBackend::Docker);
    assert_eq!(c.sandbox.network, Network::Host);
    assert_eq!(c.sandbox.image, "img:9");
    assert_eq!(c.sandbox.on_missing, OnMissing::Fail);
    assert!(!c.sandbox.enabled);
    assert_eq!(c.sandbox.remote.host, "user@box");
}

#[test]
fn env_bad_values_warn_and_skip() {
    // Malformed number / bool / enum values are ignored (defaults survive).
    let env = map_env(&[
        ("SUPERZEJ_PR_TTL", "lots"),
        ("SUPERZEJ_AUTO_REMOVE_WORKTREE", "maybe"),
        ("SUPERZEJ_PICKER", "telescope"),
        ("SUPERZEJ_REPO_SCAN_DEPTH", "deep"),
    ]);
    let o = env_overlay(&env);
    assert_eq!(o.pr_ttl_secs, None);
    assert_eq!(o.auto_remove_worktree, None);
    assert_eq!(o.picker, None);
    assert_eq!(o.repo_scan_depth, None);
    // parse_bool accepts the documented spellings.
    assert_eq!(parse_bool("on", "k"), Some(true));
    assert_eq!(parse_bool("0", "k"), Some(false));
    assert_eq!(parse_bool("huh", "k"), None);
}

#[test]
fn get_dotted_covers_all_keys() {
    let c = Config::default();
    for key in [
        "worktrees_dir",
        "workspaces_dir",
        "base_branch",
        "branch_prefix",
        "picker",
        "worktree_mode",
        "name_scheme",
        "auto_remove_worktree",
        "repo_scan_depth",
        "repo_roots",
        "theme.accent",
        "pr.ttl_secs",
        "watch.pr_interval_secs",
        "metrics.interval_secs",
        "metrics.timeout_ms",
        "metrics.max_body_bytes",
        "log.level",
        "log.file",
        "log.dir",
        "log.rotation_size_mb",
        "log.max_files",
        "log.format",
        "sandbox.enabled",
        "sandbox.backend",
        "sandbox.image",
        "sandbox.network",
        "sandbox.on_missing",
        "sandbox.remote.host",
        "sandbox.remote.transport",
        "sandbox.remote.mode",
    ] {
        assert!(c.get_dotted(key).is_some(), "missing dotted key: {key}");
    }
}

#[test]
fn validate_str_flags_every_section() {
    assert!(
        validate_str("not = valid = toml")
            .iter()
            .any(|e| e.contains("syntax"))
    );
    let body = "\
picker = \"x\"
worktree_mode = \"y\"
name_scheme = \"z\"
[sandbox]
backend = \"bad\"
network = \"bad\"
on_missing = \"bad\"
[sandbox.remote]
transport = \"bad\"
mode = \"bad\"
[log]
level = \"bad\"
format = \"bad\"
";
    let errs = validate_str(body);
    assert_eq!(errs.len(), 10, "{errs:?}");
    assert!(validate_str("picker = \"auto\"\n").is_empty());
    // a non-table top-level is tolerated (no panic).
    assert!(validate_str("").is_empty());
}

#[test]
fn accent_and_log_dir_helpers() {
    let mut c = Config::default();
    assert_eq!(c.accent_hex(), "#6ee7d8");
    assert!(c.accent_rgb().contains(';'));
    c.theme.accent = "#fff".into();
    assert_eq!(c.accent_rgb(), "255;255;255"); // 3-digit hex expands
    c.theme.accent = "garbage".into();
    assert_eq!(c.accent_hex(), "#6ee7d8"); // invalid falls back
    assert!(c.accent_rgb().len() > 3);
    // log dir: default vs explicit.
    assert!(c.log.dir_path().ends_with("superzej/logs"));
    c.log.dir = "~/x".into();
    assert!(!c.log.dir_path().to_string_lossy().contains('~'));
    assert!(!c.sandbox.remote.is_remote());
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn non_default_enums_roundtrip() {
    // Exercises Serialize (as_str) for the non-default variants.
    let mut c = Config::default();
    c.picker = Picker::Select;
    c.worktree_mode = WorktreeMode::InRepo;
    c.name_scheme = NameScheme::Numbered;
    c.sandbox.backend = SandboxBackend::Podman;
    c.sandbox.network = Network::None;
    c.sandbox.on_missing = OnMissing::Prompt;
    c.sandbox.remote.transport = RemoteTransport::Ssh;
    c.sandbox.remote.mode = RemoteMode::Sshfs;
    c.log.level = LogLevel::Trace;
    c.log.format = LogFormat::Json;
    let s = toml::to_string_pretty(&c).unwrap();
    let back: Config = toml::from_str(&s).unwrap();
    assert_eq!(back.sandbox.remote.transport, RemoteTransport::Ssh);
    assert_eq!(back.sandbox.remote.mode, RemoteMode::Sshfs);
    assert_eq!(back.log.level, LogLevel::Trace);
    assert_eq!(back.log.format, LogFormat::Json);
    assert_eq!(back.sandbox.on_missing, OnMissing::Prompt);
}

#[test]
fn malformed_toml_falls_back_to_defaults() {
    let dir = tmpdir("bad");
    let f = dir.join("c.toml");
    std::fs::write(&f, "this is = = not toml\n").unwrap();
    let c = Config::load_layered(&MapEnv::default(), &[], Some(f));
    assert_eq!(c.picker, Picker::Auto);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn path_is_under_xdg_config() {
    assert!(Config::path().ends_with("superzej/config.toml"));
}

#[test]
fn repo_sandbox_expands_mount_tildes() {
    let cfg = Config::default();
    let dir = tmpdir("mounts");
    let sb = cfg.repo_sandbox(&dir);
    // default mount "~/.gitconfig:ro" → tilde expanded, :ro preserved.
    assert!(
        sb.mounts
            .iter()
            .any(|m| m.ends_with("/.gitconfig:ro") && !m.starts_with('~'))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// A repo overlay that sets *every* sandbox + remote field exercises all the
// overlay `apply` branches.
#[test]
fn agent_command() {
    let mut cfg = Config::default();
    cfg.agents.push(crate::config::NamedCommand {
        name: "test".into(),
        command: "echo test".into(),
        hints: vec![],
        provider: None,
    });
    assert_eq!(cfg.agent_command("test"), Some("echo test"));
    assert_eq!(cfg.agent_command("missing"), None);
}

#[test]
fn tool_command() {
    let mut cfg = Config::default();
    cfg.tools.push(crate::config::NamedCommand {
        name: "test".into(),
        command: "echo test".into(),
        hints: vec![],
        provider: None,
    });
    assert_eq!(cfg.tool_command("test"), Some("echo test"));
    assert_eq!(cfg.tool_command("missing"), None);
}

#[test]
fn tasks_parse_and_filter_tests() {
    let cfg: Config = toml::from_str(
        r#"
            [[tasks]]
            name = "unit"
            command = "cargo"
            args = ["test"]
            kind = "test"
            matcher = "cargo-test"

            [[tasks]]
            name = "serve"
            command = "npm run dev"
            kind = "run"
            "#,
    )
    .unwrap();
    assert_eq!(cfg.tasks.len(), 2);
    let tests = cfg.test_tasks();
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0].name, "unit");
    assert_eq!(tests[0].matcher.as_deref(), Some("cargo-test"));
}

#[test]
fn pin_and_pin_by_index() {
    let mut cfg = Config::default();
    cfg.pins.push(crate::config::Pin {
        name: "test".into(),
        command: "echo test".into(),
        scope: crate::config::PinScope::Global,
        workspace: None,
        cwd: None,
        start: crate::config::PinStart::Lazy,
        restart: crate::config::PinRestart::Never,
        singleton: false,
        location: crate::config::PinLocation::Tab,
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        label: None,
        ratio: None,
        corner: crate::config::PinCorner::BottomRight,
        corner_width: None,
        corner_height: None,
    });
    assert_eq!(cfg.pin("test").unwrap().name, "test");
    assert!(cfg.pin("missing").is_none());
    assert_eq!(cfg.pin_by_index(1).unwrap().name, "test");
    assert!(cfg.pin_by_index(0).is_none());
    assert!(cfg.pin_by_index(2).is_none());
}

#[test]
fn pins_for_workspace() {
    let mut cfg = Config::default();
    cfg.pins.push(crate::config::Pin {
        name: "global".into(),
        command: "echo test".into(),
        scope: crate::config::PinScope::Global,
        workspace: None,
        cwd: None,
        start: crate::config::PinStart::Lazy,
        restart: crate::config::PinRestart::Never,
        singleton: false,
        location: crate::config::PinLocation::Tab,
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        label: None,
        ratio: None,
        corner: crate::config::PinCorner::BottomRight,
        corner_width: None,
        corner_height: None,
    });
    cfg.pins.push(crate::config::Pin {
        name: "local".into(),
        command: "echo test".into(),
        scope: crate::config::PinScope::Workspace,
        workspace: Some("repo".into()),
        cwd: None,
        start: crate::config::PinStart::Lazy,
        restart: crate::config::PinRestart::Never,
        singleton: false,
        location: crate::config::PinLocation::Tab,
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        label: None,
        ratio: None,
        corner: crate::config::PinCorner::BottomRight,
        corner_width: None,
        corner_height: None,
    });
    cfg.pins.push(crate::config::Pin {
        name: "local_any".into(),
        command: "echo test".into(),
        scope: crate::config::PinScope::Workspace,
        workspace: None,
        cwd: None,
        start: crate::config::PinStart::Lazy,
        restart: crate::config::PinRestart::Never,
        singleton: false,
        location: crate::config::PinLocation::Tab,
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        label: None,
        ratio: None,
        corner: crate::config::PinCorner::BottomRight,
        corner_width: None,
        corner_height: None,
    });
    let none_pins = cfg.pins_for_workspace(None);
    assert_eq!(none_pins.len(), 1); // just global
    assert!(none_pins.iter().any(|p| p.name == "global"));
    let some_pins = cfg.pins_for_workspace(Some("repo"));
    assert_eq!(some_pins.len(), 2); // global, local
}

#[test]
fn full_repo_overlay_applies_every_field() {
    let cfg = Config::default();
    let dir = tmpdir("full");
    std::fs::write(
        dir.join(".superzej.toml"),
        "\
[sandbox]
enabled = false
backend = \"docker\"
backend_chain = [\"docker\", \"none\"]
image = \"img:2\"
network = \"none\"
env_passthrough = [\"FOO\"]
mounts = [\"/a:/b\"]
init_script = \"echo go\"
devenv = true
on_missing = \"fail\"
[sandbox.remote]
host = \"u@h\"
port = 2200
transport = \"ssh\"
mode = \"sshfs\"
remote_dir = \"/srv/wt\"
forward_agent = false
",
    )
    .unwrap();
    // Post-clamp (the security fix): a repo overlay is a *clamped request*.
    // Weakenings denied; tightenings granted; additive fields gated.
    let resolved = cfg.repo_sandbox_resolved(&dir, &crate::config_resolve::Approvals::deny_all());
    let sb = &resolved.sandbox;
    // Denied (stay at trusted defaults):
    assert!(sb.enabled, "repo may not disable the sandbox");
    assert_eq!(sb.backend, cfg.sandbox.backend, "repo may not set backend");
    assert_eq!(sb.backend_chain, cfg.sandbox.backend_chain);
    assert_eq!(sb.env_passthrough, cfg.sandbox.env_passthrough);
    assert_eq!(sb.remote.host, "", "repo may not set a remote host");
    // Gated (not applied without approval):
    assert_eq!(sb.image, cfg.sandbox.image, "image is TOFU-gated");
    assert!(
        !sb.mounts.iter().any(|m| m == "/a:/b"),
        "requested mount is TOFU-gated, not applied"
    );
    // Granted (tightening / preference):
    assert_eq!(sb.network, Network::None, "tightening egress is granted");
    assert_eq!(sb.on_missing, OnMissing::Fail, "on_missing may tighten");
    assert!(sb.devenv, "in-sandbox preference passes through");
    // Denials + pending are surfaced, never silent.
    assert!(resolved.events.iter().any(|e| e.key == "sandbox.enabled"));
    assert!(resolved.events.iter().any(|e| e.key == "sandbox.backend"));
    assert!(resolved.events.iter().any(|e| e.key == "sandbox.remote"));
    assert!(resolved.pending.iter().any(|p| p.key == "sandbox.image"));
    assert!(resolved.pending.iter().any(|p| p.key == "sandbox.mounts"));
    let _ = std::fs::remove_dir_all(&dir);
}

// --- named execution environments (`[env.<name>]`) -----------------------

#[test]
fn default_env_reproduces_legacy_behavior() {
    // No [env.*] defined → the implicit "default" env: base [sandbox] +
    // a placement derived from [sandbox.remote] + the GitLoc (today's path).
    let cfg = Config::default();
    let dir = tmpdir("env-default");
    let loc = GitLoc::Local(dir.clone());
    let env = cfg.resolve_env(&dir, &loc, &dir, None);
    assert_eq!(env.name, "default");
    assert!(env.placement.is_local());
    assert!(!env.is_remote());
    // The resolved sandbox equals repo_sandbox (modulo identical content).
    assert_eq!(env.sandbox.backend, cfg.repo_sandbox(&dir).backend);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn named_env_overlays_isolation_and_local_placement() {
    let cfg: Config = toml::from_str(
        "\
[env.local-containers]
placement = \"local\"
[env.local-containers.sandbox]
backend = \"podman\"
image = \"registry.example.com/dev:latest\"
profile = \"sealed\"
",
    )
    .unwrap();
    let dir = tmpdir("env-local");
    let loc = GitLoc::Local(dir.clone());
    let env = cfg.resolve_env(&dir, &loc, &dir, Some("local-containers"));
    assert_eq!(env.name, "local-containers");
    assert!(env.placement.is_local());
    assert_eq!(env.sandbox.backend, SandboxBackend::Podman);
    assert_eq!(env.sandbox.image, "registry.example.com/dev:latest");
    assert_eq!(env.sandbox.profile, SandboxProfile::Sealed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn provider_connect_parse_and_default() {
    assert_eq!(ProviderConnect::default(), ProviderConnect::Exec);
    assert_eq!(
        ProviderConnect::from_str_validated("ssh"),
        Ok(ProviderConnect::Ssh)
    );
    assert_eq!(
        ProviderConnect::from_str_validated("exec"),
        Ok(ProviderConnect::Exec)
    );
    assert!(ProviderConnect::from_str_validated("nope").is_err());
    assert_eq!(ProviderConnect::Ssh.as_str(), "ssh");
    // Parses from an env provider table.
    let cfg: Config = toml::from_str(
            "[env.x]\nplacement = \"provider\"\n[env.x.provider]\nprovider = \"sprites\"\nconnect = \"ssh\"\n",
        )
        .unwrap();
    assert_eq!(cfg.env["x"].provider.connect, ProviderConnect::Ssh);
}

#[test]
fn home_config_default_is_portable_and_safe() {
    let h = HomeConfig::default();
    assert_eq!(h.strategy, ShellStrategy::Portable);
    assert!(
        h.portable_dotfiles_only,
        "safe default: drop non-portable rc"
    );
    assert!(!h.is_enabled(), "strategy alone is not 'enabled'");
}

#[test]
fn sandbox_overlay_merges_home_strategy_per_env() {
    // Global portable; one env asks for host-parity, another for clean.
    let cfg: Config = toml::from_str(
        "\
[sandbox.home]
strategy = \"portable\"
tools = [\"fd\", \"fzf\"]
[env.bigbox]
placement = \"local\"
[env.bigbox.sandbox.home]
strategy = \"host-parity\"
[env.sprite]
placement = \"local\"
[env.sprite.sandbox.home]
strategy = \"clean\"
",
    )
    .unwrap();
    let dir = tmpdir("env-home");
    let loc = GitLoc::Local(dir.clone());
    let big = cfg.resolve_env(&dir, &loc, &dir, Some("bigbox"));
    let sprite = cfg.resolve_env(&dir, &loc, &dir, Some("sprite"));
    let dflt = cfg.resolve_env(&dir, &loc, &dir, Some("nope-default"));
    assert_eq!(big.sandbox.home.strategy, ShellStrategy::HostParity);
    assert_eq!(sprite.sandbox.home.strategy, ShellStrategy::Clean);
    assert_eq!(dflt.sandbox.home.strategy, ShellStrategy::Portable);
    // Field-merge: the override only set `strategy`, so global `tools` inherit.
    assert_eq!(
        big.sandbox.home.tools,
        vec!["fd".to_string(), "fzf".to_string()]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn home_overlay_apply_field_merges_present_only() {
    let mut base = HomeConfig {
        tools: vec!["fd".into()],
        strategy: ShellStrategy::Portable,
        portable_dotfiles_only: true,
        ..HomeConfig::default()
    };
    let ov = HomeOverlay {
        strategy: Some(ShellStrategy::Clean),
        ..HomeOverlay::default()
    };
    ov.apply(&mut base);
    assert_eq!(base.strategy, ShellStrategy::Clean); // present → replaced
    assert_eq!(base.tools, vec!["fd".to_string()]); // absent → inherited
    assert!(base.portable_dotfiles_only); // absent → inherited
}

#[test]
fn home_overlay_merges_atuin_and_is_enabled_counts_it() {
    // Per-env opt-in: present overrides, absent inherits.
    let mut base = HomeConfig::default();
    assert!(!base.atuin && !base.is_enabled(), "off by default");
    HomeOverlay {
        atuin: Some(true),
        ..HomeOverlay::default()
    }
    .apply(&mut base);
    assert!(base.atuin, "overlay turns atuin on");
    assert!(base.is_enabled(), "atuin alone enables the personal layer");
    // An overlay without `atuin` leaves the base value untouched.
    let mut on = HomeConfig {
        atuin: true,
        ..HomeConfig::default()
    };
    HomeOverlay::default().apply(&mut on);
    assert!(on.atuin, "absent overlay key inherits the base");
    assert!(HomeOverlay::default().atuin.is_none());
}

#[test]
fn sandbox_overlay_is_empty_accounts_for_home() {
    let empty = SandboxOverlay::default();
    assert!(empty.is_empty());
    let with_home = SandboxOverlay {
        home: Some(HomeOverlay {
            strategy: Some(ShellStrategy::Clean),
            ..HomeOverlay::default()
        }),
        ..SandboxOverlay::default()
    };
    assert!(
        !with_home.is_empty(),
        "a home strategy override makes it non-empty"
    );
    // An all-None HomeOverlay does NOT make the overlay non-empty.
    let blank_home = SandboxOverlay {
        home: Some(HomeOverlay::default()),
        ..SandboxOverlay::default()
    };
    assert!(blank_home.is_empty());
}

#[test]
fn k8s_env_builds_kubectl_placement() {
    let cfg: Config = toml::from_str(
        "\
[env.company-k8s]
placement = \"k8s\"
[env.company-k8s.sandbox]
backend = \"none\"
[env.company-k8s.k8s]
context = \"company-prod\"
namespace = \"dev-blake\"
pod = \"sz-dev\"
",
    )
    .unwrap();
    let dir = tmpdir("env-k8s");
    let loc = GitLoc::Local(dir.clone());
    let env = cfg.resolve_env(&dir, &loc, &dir, Some("company-k8s"));
    assert!(env.is_remote());
    assert_eq!(env.placement.label(), "k8s:dev-blake/sz-dev");
    // The kubectl exec argv carries the configured context/namespace/pod.
    let argv = env.placement.interactive_argv(&["true".into()]);
    assert_eq!(argv[0], "kubectl");
    assert!(argv.windows(2).any(|w| w == ["--context", "company-prod"]));
    assert!(argv.windows(2).any(|w| w == ["--namespace", "dev-blake"]));
    assert!(argv.contains(&"sz-dev".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn provider_env_substitutes_id_into_exec_template() {
    let cfg: Config = toml::from_str(
        "\
[env.daytona]
placement = \"provider\"
[env.daytona.provider]
provider = \"daytona\"
id = \"sb-42\"
exec_command = [\"daytona\", \"ssh\", \"{id}\", \"--\"]
",
    )
    .unwrap();
    let dir = tmpdir("env-prov");
    let loc = GitLoc::Local(dir.clone());
    let env = cfg.resolve_env(&dir, &loc, &dir, Some("daytona"));
    assert!(env.is_remote());
    let argv = env.placement.interactive_argv(&["ls".into()]);
    assert_eq!(&argv[..4], &["daytona", "ssh", "sb-42", "--"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn provider_env_lifecycle_commands_substitute_id() {
    let cfg: Config = toml::from_str(
        "\
[env.daytona]
placement = \"provider\"
[env.daytona.provider]
provider = \"daytona\"
id = \"sb-7\"
exec_command = [\"daytona\", \"ssh\", \"{id}\", \"--\"]
up_command = [\"daytona\", \"create\", \"--id\", \"{id}\"]
down_command = [\"daytona\", \"delete\", \"{id}\"]
",
    )
    .unwrap();
    let dir = tmpdir("env-prov-life");
    let loc = GitLoc::Local(dir.clone());
    let env = cfg.resolve_env(&dir, &loc, &dir, Some("daytona"));
    match env.placement {
        crate::placement::Placement::Provider(p) => {
            assert_eq!(p.up_command, vec!["daytona", "create", "--id", "sb-7"]);
            assert_eq!(p.down_command, vec!["daytona", "delete", "sb-7"]);
        }
        other => panic!("expected provider placement, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn env_selection_precedence_repo_over_global_default() {
    // Global default_env picks "g"; a repo .superzej.toml `env = "r"` wins;
    // an explicit `selected` beats both.
    let cfg: Config = toml::from_str(
        "\
[sandbox]
default_env = \"g\"
[env.g]
[env.g.sandbox]
backend = \"bwrap\"
[env.r]
[env.r.sandbox]
backend = \"docker\"
[env.x]
[env.x.sandbox]
backend = \"podman\"
",
    )
    .unwrap();
    let dir = tmpdir("env-prec");
    std::fs::write(dir.join(".superzej.toml"), "env = \"r\"\n").unwrap();
    let loc = GitLoc::Local(dir.clone());
    // No explicit selection → repo overlay "r" wins over global default "g".
    assert_eq!(cfg.resolve_env(&dir, &loc, &dir, None).name, "r");
    assert_eq!(
        cfg.resolve_env(&dir, &loc, &dir, None).sandbox.backend,
        SandboxBackend::Docker
    );
    // Explicit selection beats the repo overlay.
    assert_eq!(cfg.resolve_env(&dir, &loc, &dir, Some("x")).name, "x");
    // Empty/whitespace selection is ignored (falls through to repo).
    assert_eq!(cfg.resolve_env(&dir, &loc, &dir, Some("  ")).name, "r");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_env_name_falls_back_to_default() {
    let cfg = Config::default();
    let dir = tmpdir("env-unknown");
    let loc = GitLoc::Local(dir.clone());
    let env = cfg.resolve_env(&dir, &loc, &dir, Some("does-not-exist"));
    assert_eq!(env.name, "default");
    assert!(env.placement.is_local());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ssh_env_falls_back_to_global_remote_host() {
    // [env.*.ssh] with no host inherits [sandbox.remote] host.
    let cfg: Config = toml::from_str(
        "\
[sandbox.remote]
host = \"u@devbox\"
port = 2200
[env.remote-dev]
placement = \"ssh\"
[env.remote-dev.ssh]
transport = \"ssh\"
",
    )
    .unwrap();
    let dir = tmpdir("env-ssh");
    let loc = GitLoc::Local(dir.clone());
    let env = cfg.resolve_env(&dir, &loc, &dir, Some("remote-dev"));
    assert!(env.is_remote());
    assert_eq!(env.placement.label(), "ssh:u@devbox");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn process_env_reads_real_vars() {
    // SAFETY: single-threaded test using uniquely-named vars.
    unsafe { std::env::set_var("SUPERZEJ_TEST_PENV_xyz", "v1") };
    assert_eq!(
        ProcessEnv.get("SUPERZEJ_TEST_PENV_xyz").as_deref(),
        Some("v1")
    );
    assert!(ProcessEnv.get("SUPERZEJ_TEST_PENV_absent_qqq").is_none());
    unsafe { std::env::remove_var("SUPERZEJ_TEST_PENV_xyz") };
    // blank values are treated as unset.
    unsafe { std::env::set_var("SUPERZEJ_TEST_PENV_blank", "   ") };
    assert!(ProcessEnv.get("SUPERZEJ_TEST_PENV_blank").is_none());
    unsafe { std::env::remove_var("SUPERZEJ_TEST_PENV_blank") };
}

#[test]
fn config_parses_all_mode_specific_keybind_tables() {
    let toml = r#"
            [keybinds]
            new-worktree = "Ctrl w"

            [keybinds.vim_normal]
            focus-down = "j"

            [keybinds.vim_insert]
            mode-vim-normal = "Esc"

            [keybinds.emacs]
            focus-left = "Ctrl b"
        "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.keybinds.get("new-worktree").unwrap(), "Ctrl w");
    assert_eq!(cfg.keybinds.vim_normal.get("focus-down").unwrap(), "j");
    assert_eq!(
        cfg.keybinds.vim_insert.get("mode-vim-normal").unwrap(),
        "Esc"
    );
    assert_eq!(cfg.keybinds.emacs.get("focus-left").unwrap(), "Ctrl b");
}

#[test]
fn agent_and_tool_command_lookup_by_name() {
    let cfg: Config = toml::from_str(
        "[[agents]]\nname = 'claude'\ncommand = 'claude --foo'\n\
             [[tools]]\nname = 'lazygit'\ncommand = 'lazygit'\n",
    )
    .unwrap();
    assert_eq!(cfg.agent_command("claude"), Some("claude --foo"));
    assert_eq!(cfg.agent_command("nope"), None);
    assert_eq!(cfg.tool_command("lazygit"), Some("lazygit"));
    assert_eq!(cfg.tool_command("nope"), None);
}

#[test]
fn pin_lookup_by_name_and_index_and_workspace_scope() {
    let cfg: Config = toml::from_str(
            "[[pins]]\nname = 'aerc'\ncommand = 'aerc'\n\
             [[pins]]\nname = 'logs'\ncommand = 'journalctl -f'\nscope = 'workspace'\nworkspace = '/repo'\n",
        )
        .unwrap();
    // By name.
    assert_eq!(cfg.pin("aerc").map(|p| p.name.as_str()), Some("aerc"));
    assert!(cfg.pin("missing").is_none());
    // By 1-based index (0 and out-of-range miss).
    assert_eq!(cfg.pin_by_index(1).map(|p| p.name.as_str()), Some("aerc"));
    assert_eq!(cfg.pin_by_index(2).map(|p| p.name.as_str()), Some("logs"));
    assert!(cfg.pin_by_index(0).is_none());
    assert!(cfg.pin_by_index(3).is_none());
    // Workspace scoping: global pin always shows; workspace pin only for its repo.
    let global_only = cfg.pins_for_workspace(None);
    assert_eq!(global_only.len(), 1);
    assert_eq!(global_only[0].name, "aerc");
    let in_repo = cfg.pins_for_workspace(Some("/repo"));
    assert_eq!(in_repo.len(), 2);
}

#[test]
fn expand_env_ref_resolves_env_prefix() {
    unsafe { std::env::set_var("SUPERZEJ_TEST_EXPAND_TOKEN", "secret") };
    assert_eq!(
        expand_env_ref("env:SUPERZEJ_TEST_EXPAND_TOKEN"),
        Some("secret".into())
    );
    unsafe { std::env::remove_var("SUPERZEJ_TEST_EXPAND_TOKEN") };
    // Missing var returns None.
    assert_eq!(expand_env_ref("env:SUPERZEJ_TEST_EXPAND_TOKEN"), None);
}

#[test]
fn expand_env_ref_returns_literal_for_plain_value() {
    assert_eq!(expand_env_ref("lin_abc123"), Some("lin_abc123".into()));
}

#[test]
fn profile_toml_overlay_merges_over_base_and_preserves_untouched() {
    let mut cfg = Config {
        branch_prefix: "sz/".into(),
        ..Config::default()
    };
    let base_accent = cfg.theme.accent.clone();
    // A profile overlay changes branch_prefix + a nested sandbox field, and
    // leaves theme.accent untouched.
    Config::apply_toml_overlay(
        &mut cfg,
        "branch_prefix = \"work/\"\n[sandbox]\nnetwork = \"none\"\n",
    )
    .unwrap();
    assert_eq!(cfg.branch_prefix, "work/", "overlay wins");
    assert_eq!(cfg.sandbox.network, Network::None, "nested overlay applies");
    assert_eq!(
        cfg.theme.accent, base_accent,
        "untouched base key preserved"
    );
}

#[test]
fn profile_overlay_path_none_for_default_some_for_named() {
    struct FakeEnv(Option<String>);
    impl EnvSource for FakeEnv {
        fn get(&self, k: &str) -> Option<String> {
            (k == "SUPERZEJ_PROFILE").then(|| self.0.clone()).flatten()
        }
    }
    assert!(Config::profile_overlay_path(&FakeEnv(None)).is_none());
    assert!(Config::profile_overlay_path(&FakeEnv(Some("default".into()))).is_none());
    let p = Config::profile_overlay_path(&FakeEnv(Some("work".into()))).unwrap();
    assert!(p.ends_with("superzej/profiles/work/config.toml"));
}

#[path = "config_tests_coverage.rs"]
mod coverage;
