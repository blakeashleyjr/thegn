use super::super::*;
use super::{map_env, tmpdir};

#[test]
fn expand_env_ref_returns_none_for_empty() {
    assert_eq!(expand_env_ref(""), None);
    assert_eq!(expand_env_ref("   "), None);
}

#[test]
fn issue_provider_kind_infallible_deserialize() {
    let k: IssueProviderKind = serde_json::from_str(r#""linear""#).unwrap();
    assert_eq!(k, IssueProviderKind::Linear);
    // Unknown value falls back to default (None) without panic.
    let k: IssueProviderKind = serde_json::from_str(r#""unknown_provider""#).unwrap();
    assert_eq!(k, IssueProviderKind::None);
}

#[test]
fn issues_config_defaults() {
    let cfg = Config::default();
    assert_eq!(cfg.issues.provider, IssueProviderKind::None);
    assert_eq!(cfg.issues.ttl_secs, 60);
    assert_eq!(cfg.issues.max_issues, 100);
    assert!(cfg.issues.filter_assignee_me);
    assert_eq!(cfg.issues.linear.api_key, "env:LINEAR_API_KEY");
    assert_eq!(cfg.issues.jira.api_token, "env:JIRA_API_TOKEN");
    assert!(cfg.issues.providers.is_empty());
    assert!(cfg.issues.active_providers().is_empty());
}

#[test]
fn active_providers_back_compat_single() {
    // Legacy single `provider` is honored when `providers` is empty.
    let mut cfg = IssuesConfig {
        provider: IssueProviderKind::Linear,
        ..Default::default()
    };
    assert_eq!(cfg.active_providers(), vec![IssueProviderKind::Linear]);
    // `none` yields an empty set, not a [None] entry.
    cfg.provider = IssueProviderKind::None;
    assert!(cfg.active_providers().is_empty());
}

#[test]
fn active_providers_multi_wins_and_dedups() {
    // Single provider is overridden once the plural list is set.
    let cfg = IssuesConfig {
        provider: IssueProviderKind::Github,
        providers: vec![
            IssueProviderKind::Linear,
            IssueProviderKind::Jira,
            IssueProviderKind::Linear, // duplicate collapses
            IssueProviderKind::None,   // None filtered out
        ],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_providers(),
        vec![IssueProviderKind::Linear, IssueProviderKind::Jira]
    );
}

#[test]
fn issues_multi_provider_table_parses() {
    let toml = r#"
            [issues]
            providers = ["linear", "jira"]
        "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(
        cfg.issues.providers,
        vec![IssueProviderKind::Linear, IssueProviderKind::Jira]
    );
    assert_eq!(
        cfg.issues.active_providers(),
        vec![IssueProviderKind::Linear, IssueProviderKind::Jira]
    );
}

#[test]
fn notification_priority_defaults_and_overrides() {
    use crate::notification::{NotificationKind, Priority};
    let mut cfg = NotificationsConfig::default();

    // Defaults: failures alert, lifecycle info, the rest notice.
    assert_eq!(
        cfg.priority_of(NotificationKind::TestFailed),
        Priority::Alert
    );
    assert_eq!(
        cfg.priority_of(NotificationKind::WorktreeCreated),
        Priority::Info
    );
    assert_eq!(
        cfg.priority_of(NotificationKind::AgentDone),
        Priority::Notice
    );

    // Alert set: failures + agent-attention + the queue's give-up; unread
    // excludes Info.
    let alerts = cfg.alert_kind_names();
    assert_eq!(alerts.len(), 6);
    let want = "agent_failed agent_attention test_failed log_error \
                    process_failed queue_needs_human";
    for k in want.split_whitespace() {
        assert!(alerts.contains(&k), "missing {k}");
    }
    let counted = cfg.counted_unread_kind_names();
    assert!(!counted.contains(&"worktree_created"));
    assert!(!counted.contains(&"process_exited"));
    assert!(counted.contains(&"test_failed") && counted.contains(&"agent_done"));

    // Override: garbage falls back to default; a demotion reclassifies live.
    cfg.priority.insert("test_failed".into(), "garbage".into());
    assert_eq!(
        cfg.priority_of(NotificationKind::TestFailed),
        Priority::Alert
    );
    cfg.priority.insert("test_failed".into(), "notice".into());
    assert_eq!(
        cfg.priority_of(NotificationKind::TestFailed),
        Priority::Notice
    );
    assert!(!cfg.alert_kind_names().contains(&"test_failed"));
    assert!(cfg.counted_unread_kind_names().contains(&"test_failed"));
    // Promote an info kind to alert.
    cfg.priority
        .insert("worktree_created".into(), "alert".into());
    assert!(cfg.alert_kind_names().contains(&"worktree_created"));
}

#[test]
fn notification_routing_config_parses() {
    let toml = r#"
[notifications]
active_mode = "focus"

[notifications.dnd]
enabled = true
windows = ["22:00-08:00", "sat,sun 00:00-24:00"]
allow_priority = "notice"

[notifications.sound]
mode = "command"
min_priority = "notice"
command = "paplay alert.oga"

[notifications.sound.per_priority]
alert = "paplay crit.oga"

[notifications.modes.focus]
label = "Heads down"

[[notifications.rules]]
name = "mute noisy"
worktree = "*/scratch"
mute = true

[[notifications.rules]]
kind = "agent_done"
set_priority = "alert"
stop = true
"#;
    let cfg: Config = toml::from_str(toml).expect("parses");
    let n = &cfg.notifications;
    assert_eq!(n.active_mode, "focus");
    assert!(n.dnd.enabled);
    assert_eq!(n.dnd.windows.len(), 2);
    assert_eq!(n.dnd.allow_priority, "notice");
    assert_eq!(n.sound.mode, SoundMode::Command);
    assert_eq!(n.sound.command, "paplay alert.oga");
    assert_eq!(
        n.sound.per_priority.get("alert").unwrap(),
        "paplay crit.oga"
    );
    assert!(n.modes.contains_key("focus"));
    assert_eq!(n.rules.len(), 2);
    assert_eq!(n.rules[0].worktree.as_deref(), Some("*/scratch"));
    assert!(n.rules[0].mute);
    assert_eq!(n.rules[1].kind.as_deref(), Some("agent_done"));
    assert!(n.rules[1].stop);
}

#[test]
fn notification_bad_sound_mode_warns_and_defaults() {
    let cfg: Config = toml::from_str(
        r#"
[notifications.sound]
mode = "bogus"
"#,
    )
    .expect("parses");
    // Unknown enum value warns and falls back to the default (Bell).
    assert_eq!(cfg.notifications.sound.mode, SoundMode::Bell);
}

#[test]
fn profile_notifications_overlay_layers() {
    let toml = r#"
profile = "work"

[notifications]
desktop = true
active_mode = "all"

[notifications.sound]
mode = "bell"

[profiles.work.notifications]
active_mode = "focus"

[profiles.work.notifications.sound]
mode = "off"
min_priority = "alert"
"#;
    let cfg: Config = toml::from_str(toml).expect("parses");
    // Global untouched.
    assert_eq!(cfg.notifications.active_mode, "all");
    assert_eq!(cfg.notifications.sound.mode, SoundMode::Bell);
    // Effective (no repo root) applies the active profile overlay.
    let eff = cfg.effective_notifications(None);
    assert_eq!(eff.active_mode, "focus");
    assert_eq!(eff.sound.mode, SoundMode::Off);
    // A field the overlay didn't set inherits the global value.
    assert!(eff.desktop);
}

#[test]
fn effective_notifications_no_profile_is_identity() {
    let cfg = Config::default();
    let eff = cfg.effective_notifications(None);
    assert_eq!(eff.active_mode, cfg.notifications.active_mode);
    assert_eq!(eff.sound.mode, cfg.notifications.sound.mode);
}

#[test]
fn notifications_overlay_apply_covers_every_field() {
    // is_empty on the default overlay.
    assert!(NotificationsOverlay::default().is_empty());

    let mut base = NotificationsConfig::default();
    let mut priority = std::collections::BTreeMap::new();
    priority.insert("agent_done".to_string(), "alert".to_string());
    let mut modes = std::collections::BTreeMap::new();
    modes.insert("focus".to_string(), NotificationMode::default());
    let overlay = NotificationsOverlay {
        desktop: Some(false),
        desktop_min_urgency: Some("critical".into()),
        process_exit: Some("all".into()),
        priority: Some(priority),
        rules: Some(vec![NotificationRule {
            drop: true,
            ..Default::default()
        }]),
        dnd: Some(DndConfig {
            enabled: true,
            windows: vec!["22:00-08:00".into()],
            allow_priority: "notice".into(),
        }),
        sound: Some(SoundConfig {
            mode: SoundMode::Off,
            ..Default::default()
        }),
        modes: Some(modes),
        active_mode: Some("focus".into()),
    };
    assert!(!overlay.is_empty());
    overlay.apply(&mut base);
    assert!(!base.desktop);
    assert_eq!(base.desktop_min_urgency, "critical");
    assert_eq!(base.process_exit, "all");
    assert_eq!(base.priority.get("agent_done").unwrap(), "alert");
    assert!(base.has_rules());
    assert!(base.dnd.enabled);
    assert_eq!(base.sound.mode, SoundMode::Off);
    assert!(base.modes.contains_key("focus"));
    assert_eq!(base.active_mode, "focus");
}

#[test]
fn llm_proxy_disabled_by_default_no_launch() {
    let cfg = Config::default();
    assert!(!cfg.llm_proxy.enabled);
    assert_eq!(cfg.llm_proxy.listen, "127.0.0.1:8383");
    assert_eq!(cfg.llm_proxy.routing, RoutingStrategy::Sequential);
    assert!(cfg.llm_proxy.launch_spec().is_none());
}

#[test]
fn llm_proxy_launch_spec_when_enabled() {
    let mut cfg = LlmProxyConfig {
        enabled: true,
        config_path: "/etc/szproxy.json".into(),
        ..Default::default()
    };
    let (prog, args, env) = cfg.launch_spec().unwrap();
    assert_eq!(prog, "szproxy");
    assert!(args.is_empty());
    assert_eq!(env.get("SZPROXY_LISTEN").unwrap(), "127.0.0.1:8383");
    assert_eq!(env.get("SZPROXY_CONFIG").unwrap(), "/etc/szproxy.json");
    // No config path → no SZPROXY_CONFIG env entry.
    cfg.config_path = String::new();
    let (_, _, env) = cfg.launch_spec().unwrap();
    assert!(!env.contains_key("SZPROXY_CONFIG"));
}

#[test]
fn routing_strategy_aliases_and_fallback() {
    assert_eq!(
        RoutingStrategy::from_str_validated("failover").unwrap(),
        RoutingStrategy::Sequential
    );
    assert_eq!(
        RoutingStrategy::from_str_validated("cascade").unwrap(),
        RoutingStrategy::Speculative
    );
    // Unknown deserializes to the default without panic.
    let s: RoutingStrategy = serde_json::from_str(r#""nonsense""#).unwrap();
    assert_eq!(s, RoutingStrategy::Sequential);
}

// ---- config_enum! Default + Display + from_str_validated round-trips ----

#[test]
fn config_enum_defaults_and_displays() {
    // Default variant matches the macro `default =` clause for every enum.
    assert_eq!(Picker::default(), Picker::Auto);
    assert_eq!(UndercurlMode::default(), UndercurlMode::Auto);
    assert_eq!(WorktreeMode::default(), WorktreeMode::Global);
    assert_eq!(NameScheme::default(), NameScheme::Words);
    assert_eq!(SandboxBackend::default(), SandboxBackend::Auto);
    assert_eq!(Network::default(), Network::Nat);
    assert_eq!(OnMissing::default(), OnMissing::Warn);
    assert_eq!(RemoteTransport::default(), RemoteTransport::Mosh);
    assert_eq!(RemoteMode::default(), RemoteMode::Remote);
    assert_eq!(LogLevel::default(), LogLevel::Info);
    assert_eq!(LogFormat::default(), LogFormat::Text);
    assert_eq!(PinLocation::default(), PinLocation::Tab);
    assert_eq!(PinScope::default(), PinScope::Global);
    assert_eq!(RoutingStrategy::default(), RoutingStrategy::Sequential);
    assert_eq!(CompressionLevel::default(), CompressionLevel::Conservative);
    assert_eq!(GitCmdOutput::default(), GitCmdOutput::Popup);
    assert_eq!(IssueProviderKind::default(), IssueProviderKind::None);

    // Display delegates to as_str (canonical form).
    assert_eq!(UndercurlMode::On.to_string(), "on");
    assert_eq!(UndercurlMode::Off.to_string(), "off");
    assert_eq!(WorktreeMode::InRepo.to_string(), "in_repo");
    assert_eq!(NameScheme::Numbered.to_string(), "numbered");
    assert_eq!(LogLevel::Trace.to_string(), "trace");
    assert_eq!(GitCmdOutput::Terminal.to_string(), "terminal");
    assert_eq!(GitCmdOutput::None.to_string(), "none");
    assert_eq!(CompressionLevel::Aggressive.to_string(), "aggressive");
    assert_eq!(IssueProviderKind::Jira.to_string(), "jira");
}

#[test]
fn config_enum_every_variant_roundtrips_canon_and_aliases() {
    // SandboxBackend: each canonical + alias parses to its variant; as_str
    // emits the canonical string.
    for (s, v) in [
        ("auto", SandboxBackend::Auto),
        ("podman", SandboxBackend::Podman),
        ("podman-rootless", SandboxBackend::Podman),
        ("rootless-podman", SandboxBackend::Podman),
        ("podman-rootful", SandboxBackend::PodmanRootful),
        ("rootful-podman", SandboxBackend::PodmanRootful),
        ("docker", SandboxBackend::Docker),
        ("bwrap", SandboxBackend::Bwrap),
        ("bubblewrap", SandboxBackend::Bwrap),
        ("systemd", SandboxBackend::Systemd),
        ("systemd-run", SandboxBackend::Systemd),
        ("apple", SandboxBackend::Apple),
        ("container", SandboxBackend::Apple),
        ("wsl", SandboxBackend::Wsl),
        ("none", SandboxBackend::None),
        ("host", SandboxBackend::None),
    ] {
        assert_eq!(SandboxBackend::from_str_validated(s).unwrap(), v, "{s}");
    }
    assert_eq!(SandboxBackend::Systemd.as_str(), "systemd");
    assert_eq!(SandboxBackend::Apple.as_str(), "apple");
    assert_eq!(SandboxBackend::Wsl.as_str(), "wsl");
    assert_eq!(SandboxBackend::PodmanRootful.as_str(), "podman-rootful");

    // Network / OnMissing / RemoteTransport / RemoteMode.
    for (s, v) in [
        ("nat", Network::Nat),
        ("host", Network::Host),
        ("none", Network::None),
    ] {
        assert_eq!(Network::from_str_validated(s).unwrap(), v);
        assert_eq!(v.as_str(), s);
    }
    for (s, v) in [
        ("warn", OnMissing::Warn),
        ("prompt", OnMissing::Prompt),
        ("fail", OnMissing::Fail),
    ] {
        assert_eq!(OnMissing::from_str_validated(s).unwrap(), v);
        assert_eq!(v.as_str(), s);
    }
    assert_eq!(
        RemoteTransport::from_str_validated("ssh").unwrap(),
        RemoteTransport::Ssh
    );
    assert_eq!(RemoteTransport::Mosh.as_str(), "mosh");
    for (s, v) in [
        ("remote", RemoteMode::Remote),
        ("local_exec", RemoteMode::LocalExec),
        ("sshfs", RemoteMode::Sshfs),
    ] {
        assert_eq!(RemoteMode::from_str_validated(s).unwrap(), v);
        assert_eq!(v.as_str(), s);
    }

    // LogLevel / LogFormat full sets.
    for (s, v) in [
        ("error", LogLevel::Error),
        ("warn", LogLevel::Warn),
        ("info", LogLevel::Info),
        ("debug", LogLevel::Debug),
        ("trace", LogLevel::Trace),
    ] {
        assert_eq!(LogLevel::from_str_validated(s).unwrap(), v);
        assert_eq!(v.as_str(), s);
    }
    assert_eq!(
        LogFormat::from_str_validated("json").unwrap(),
        LogFormat::Json
    );
    assert_eq!(LogFormat::Text.as_str(), "text");

    // UndercurlMode, WorktreeMode, NameScheme, GitCmdOutput, IssueProviderKind.
    for (s, v) in [
        ("auto", UndercurlMode::Auto),
        ("on", UndercurlMode::On),
        ("off", UndercurlMode::Off),
    ] {
        assert_eq!(UndercurlMode::from_str_validated(s).unwrap(), v);
        assert_eq!(v.as_str(), s);
    }
    assert_eq!(
        WorktreeMode::from_str_validated("global").unwrap(),
        WorktreeMode::Global
    );
    assert_eq!(
        NameScheme::from_str_validated("words").unwrap(),
        NameScheme::Words
    );
    for (s, v) in [
        ("none", GitCmdOutput::None),
        ("popup", GitCmdOutput::Popup),
        ("terminal", GitCmdOutput::Terminal),
    ] {
        assert_eq!(GitCmdOutput::from_str_validated(s).unwrap(), v);
        assert_eq!(v.as_str(), s);
    }
    for (s, v) in [
        ("none", IssueProviderKind::None),
        ("linear", IssueProviderKind::Linear),
        ("github", IssueProviderKind::Github),
        ("jira", IssueProviderKind::Jira),
    ] {
        assert_eq!(IssueProviderKind::from_str_validated(s).unwrap(), v);
        assert_eq!(v.as_str(), s);
    }

    // Error messages mention the kind label and the bad value.
    let e = Network::from_str_validated("bogus").unwrap_err();
    assert!(e.contains("sandbox network") && e.contains("bogus"), "{e}");
}

#[test]
fn config_enum_parsing_is_case_and_whitespace_insensitive() {
    assert_eq!(Picker::from_str_validated("  GUM ").unwrap(), Picker::Gum);
    assert_eq!(
        SandboxBackend::from_str_validated("DOCKER").unwrap(),
        SandboxBackend::Docker
    );
}

#[test]
fn pin_scope_aliases_parse() {
    for (s, v) in [
        ("global", PinScope::Global),
        ("everywhere", PinScope::Global),
        ("all", PinScope::Global),
        ("workspace", PinScope::Workspace),
        ("local", PinScope::Workspace),
    ] {
        assert_eq!(PinScope::from_str_validated(s).unwrap(), v, "{s}");
    }
    assert_eq!(PinScope::Global.as_str(), "global");
    assert_eq!(PinScope::Workspace.as_str(), "workspace");
}

#[test]
fn pin_location_aliases_parse() {
    for (s, v) in [
        ("tab", PinLocation::Tab),
        ("layout", PinLocation::Layout),
        ("pane", PinLocation::Layout),
        ("active_layout", PinLocation::Layout),
        ("active-layout", PinLocation::Layout),
        ("strip", PinLocation::Strip),
        ("top", PinLocation::Strip),
        ("top-strip", PinLocation::Strip),
        ("top_strip", PinLocation::Strip),
        ("float", PinLocation::Float),
        ("floating", PinLocation::Float),
        ("scratch", PinLocation::Float),
    ] {
        assert_eq!(PinLocation::from_str_validated(s).unwrap(), v, "{s}");
    }
}

#[test]
fn compression_level_aliases_and_serde() {
    assert_eq!(
        CompressionLevel::from_str_validated("none").unwrap(),
        CompressionLevel::Off
    );
    assert_eq!(CompressionLevel::Off.as_str(), "off");
    assert_eq!(
        CompressionLevel::from_str_validated("balanced").unwrap(),
        CompressionLevel::Balanced
    );
    // Unknown deserializes to default (Conservative) without panic.
    let c: CompressionLevel = serde_json::from_str(r#""nonsense""#).unwrap();
    assert_eq!(c, CompressionLevel::Conservative);
    // Serialize round-trips to canonical.
    assert_eq!(
        serde_json::to_string(&CompressionLevel::Aggressive).unwrap(),
        r#""aggressive""#
    );
}

// ---- Default impls (non-trivial fields) ----

#[test]
fn section_defaults_match_documented_values() {
    assert_eq!(PrConfig::default().ttl_secs, 30);
    assert_eq!(WatchConfig::default().pr_interval_secs, 20);

    let a = AppsConfig::default();
    assert_eq!(a.default_tab, "work");
    assert_eq!(a.tab_order, vec!["work"]);

    let n = NotificationsConfig::default();
    assert!(n.desktop);
    assert_eq!(n.desktop_min_urgency, "normal");
    assert_eq!(n.process_exit, "failures_and_tasks");

    let s = SearchConfig::default();
    assert_eq!(s.max_results, 1_000);

    let l = LspConfig::default();
    assert!(l.enabled);
    assert!(l.hover);
    assert!(l.servers.is_empty());

    let p = PaletteConfig::default();
    assert_eq!(p.content_max_results, 500);
    assert_eq!(p.file_max_results, 200);
    assert_eq!(p.symbol_max_results, 100);
    assert!(!p.content_search_hidden);

    assert!(PanelConfig::default().sections.is_empty());
    assert!(!GitConfig::default().override_gpg);
}

#[test]
fn media_config_defaults_and_enums() {
    let m = MediaConfig::default();
    assert!(
        m.enabled,
        "media defaults ON (additive; inert without a backend)"
    );
    assert_eq!(
        m.backend,
        MediaBackendKind::Auto,
        "media defaults to per-OS auto resolution"
    );
    assert!(m.players_priority.is_empty());
    assert_eq!(m.default_action, MediaDefaultAction::PlayPause);
    assert_eq!(m.volume_step, 0.05);
    assert_eq!(m.poll_interval_secs, 3);
    assert_eq!(m.mpv.socket, "/tmp/mpvsocket");
    assert_eq!(m.seek_step_secs, 10);
    assert_eq!(m.seek_step_video_secs, 30);
    assert!(m.show_art);
    assert!(m.overlay_on_badge_click);
    // Seek step picks the coarser cadence for video, the finer one for audio.
    use superzej_media::model::MediaKind;
    assert_eq!(
        m.seek_step(MediaKind::Audio),
        std::time::Duration::from_secs(10)
    );
    assert_eq!(
        m.seek_step(MediaKind::Video),
        std::time::Duration::from_secs(30)
    );
    assert_eq!(
        m.seek_step(MediaKind::Unknown),
        std::time::Duration::from_secs(10)
    );

    // Aliases parse; an unknown value falls back to the default (infallible).
    assert_eq!(
        MediaBackendKind::from_str_validated("dbus").unwrap(),
        MediaBackendKind::Mpris
    );
    assert_eq!(
        MediaBackendKind::from_str_validated("off").unwrap(),
        MediaBackendKind::None
    );
    assert!(MediaBackendKind::from_str_validated("winamp").is_err());
    assert_eq!(MediaBackendKind::Mpris.as_str(), "mpris");
    // New cross-platform backends parse (canon + aliases).
    assert_eq!(
        MediaBackendKind::from_str_validated("auto").unwrap(),
        MediaBackendKind::Auto
    );
    assert_eq!(
        MediaBackendKind::from_str_validated("windows").unwrap(),
        MediaBackendKind::Smtc
    );
    assert_eq!(
        MediaBackendKind::from_str_validated("osascript").unwrap(),
        MediaBackendKind::AppleScript
    );

    // Native MPD backend: alias parses, config exposes a default endpoint, and
    // resolve_opts lowers the backend + endpoint into the leaf's ResolveOpts.
    assert_eq!(
        MediaBackendKind::from_str_validated("mpc").unwrap(),
        MediaBackendKind::Mpd
    );
    assert_eq!(m.mpd.socket, "127.0.0.1:6600");
    assert!(m.mpd.password.is_none());
    let mut mpd_cfg = MediaConfig::default();
    mpd_cfg.backend = MediaBackendKind::Mpd;
    mpd_cfg.mpd.socket = "music.lan:6601".into();
    mpd_cfg.mpd.password = Some("hunter2".into());
    let opts = mpd_cfg.resolve_opts();
    assert_eq!(opts.backend, superzej_media::BackendKind::Mpd);
    assert_eq!(opts.mpd_socket, "music.lan:6601");
    assert_eq!(opts.mpd_password.as_deref(), Some("hunter2"));

    // Default Config keeps media enabled and round-trips through TOML.
    assert!(Config::default().media.enabled);
    let toml = toml::to_string(&MediaConfig::default()).unwrap();
    let back: MediaConfig = toml::from_str(&toml).unwrap();
    assert_eq!(back.backend, MediaBackendKind::Auto);
}

#[test]
fn bars_config_defaults() {
    let b = BarsConfig::default();
    assert_eq!(b.top_left, vec!["brand"]);
    assert_eq!(
        b.top_right,
        vec![
            "cpu", "mem", "disk", "gpu", "temp", "net", "battery", "date", "clock"
        ]
    );
    assert_eq!(b.bottom_left, vec!["keyhints"]);
    assert_eq!(b.bottom_right, vec!["pr", "tests", "loc", "disk", "status"]);
    assert_eq!(b.date_format, "%a %b %-d");
    assert_eq!(b.clock_format, "%H:%M");
}

#[test]
fn limits_config_defaults() {
    let l = LimitsConfig::default();
    assert_eq!(l.test_cpu_quota, "150%");
    assert_eq!(l.test_mem_max, "4G");
    assert_eq!(l.test_nice, 10);
    assert_eq!(l.test_max_parallel, 1);
    assert_eq!(l.test_timeout_secs, 1800);
    assert_eq!(l.discover_timeout_secs, 45);
    assert!(l.isolated_target_dir);
}

#[test]
fn log_config_default_and_theme_config_default() {
    let l = LogConfig::default();
    assert_eq!(l.level, LogLevel::Info);
    assert!(!l.file);
    assert_eq!(l.dir, "");
    assert_eq!(l.rotation_size_mb, 5);
    assert_eq!(l.max_files, 5);
    assert_eq!(l.format, LogFormat::Text);

    let t = ThemeConfig::default();
    assert_eq!(t.preset, "prism");
    assert_eq!(t.accent, "#6ee7d8");
    assert_eq!(t.focus_border, "#6ee7d8");
    assert_eq!(t.pane_padding, 0);
    assert_eq!(t.undercurl, UndercurlMode::Auto);
    assert_eq!(t.color, ColorMode::Auto);
    assert_eq!(t.glyphs, GlyphMode::Auto);
}

#[test]
fn color_and_glyph_modes_parse_with_aliases() {
    assert_eq!(
        ColorMode::from_str_validated("auto").unwrap(),
        ColorMode::Auto
    );
    assert_eq!(
        ColorMode::from_str_validated("24bit").unwrap(),
        ColorMode::Truecolor
    );
    assert_eq!(
        ColorMode::from_str_validated("256").unwrap(),
        ColorMode::Ansi256
    );
    assert_eq!(
        ColorMode::from_str_validated("MONO").unwrap(),
        ColorMode::None
    );
    assert!(ColorMode::from_str_validated("16bit").is_err());

    assert_eq!(
        GlyphMode::from_str_validated("ascii").unwrap(),
        GlyphMode::Ascii
    );
    assert_eq!(
        GlyphMode::from_str_validated("unicode").unwrap(),
        GlyphMode::Unicode
    );
    assert!(GlyphMode::from_str_validated("nerd").is_err());
}

#[test]
fn theme_color_glyph_env_overrides_apply() {
    let mut env = MapEnv::default();
    env.0
        .insert("SUPERZEJ_THEME_COLOR".to_string(), "16".to_string());
    env.0
        .insert("SUPERZEJ_THEME_GLYPHS".to_string(), "ascii".to_string());
    env.0.insert(
        "SUPERZEJ_THEME_AGENT_GLYPHS".to_string(),
        "symbol".to_string(),
    );
    let o = env_overlay(&env);
    assert_eq!(o.theme_color, Some(ColorMode::Ansi16));
    assert_eq!(o.theme_glyphs, Some(GlyphMode::Ascii));
    assert_eq!(o.theme_agent_glyphs, Some(AgentGlyphs::Symbol));
    let mut cfg = Config::default();
    o.apply(&mut cfg);
    assert_eq!(cfg.theme.color, ColorMode::Ansi16);
    assert_eq!(cfg.theme.glyphs, GlyphMode::Ascii);
    assert_eq!(cfg.theme.agent_glyphs, AgentGlyphs::Symbol);
}

#[test]
fn issue_provider_subconfig_defaults() {
    let lin = LinearConfig::default();
    assert_eq!(lin.api_key, "env:LINEAR_API_KEY");
    assert_eq!(lin.team_id, "");
    assert_eq!(lin.workspace_slug, "");
    let jira = JiraConfig::default();
    assert_eq!(jira.api_token, "env:JIRA_API_TOKEN");
    assert_eq!(jira.base_url, "");
    assert_eq!(jira.email, "");
    assert_eq!(jira.project_key, "");
    assert!(GitHubIssuesConfig::default().extra_flags.is_empty());
}

#[test]
fn remote_config_default_and_is_remote() {
    let r = RemoteConfig::default();
    assert_eq!(r.host, "");
    assert_eq!(r.port, 22);
    assert_eq!(r.transport, RemoteTransport::Mosh);
    assert_eq!(r.mode, RemoteMode::Remote);
    assert_eq!(r.remote_dir, "~/superzej-worktrees");
    assert!(r.forward_agent);
    assert!(!r.is_remote());
    let r2 = RemoteConfig {
        host: "  user@box ".into(),
        ..RemoteConfig::default()
    };
    assert!(r2.is_remote());
    let blank = RemoteConfig {
        host: "   ".into(),
        ..RemoteConfig::default()
    };
    assert!(!blank.is_remote());
}

#[test]
fn sandbox_config_default_collections() {
    let s = SandboxConfig::default();
    assert!(s.enabled);
    assert_eq!(s.backend, SandboxBackend::Auto);
    assert_eq!(s.default_backend, SandboxBackend::Auto);
    assert_eq!(
        s.backend_chain,
        vec![
            "podman-rootless",
            "podman-rootful",
            "docker",
            "bwrap",
            "host"
        ]
    );
    assert!(s.image.is_empty());
    assert!(s.env_passthrough.contains(&"SSH_AUTH_SOCK".to_string()));
    assert!(s.env_passthrough.contains(&"GH_TOKEN".to_string()));
    assert!(s.auto_caches);
    assert!(s.mounts.contains(&"~/.gitconfig:ro".to_string()));
    assert!(!s.devenv);
    assert_eq!(s.on_missing, OnMissing::Warn);
    assert_eq!(s.file_access, FileAccess::WorktreePlusCaches);
    assert!(s.network_allow.is_empty());
    assert!(!s.network_audit);
}

#[test]
fn file_access_default_and_serde() {
    assert_eq!(FileAccess::default(), FileAccess::WorktreePlusCaches);
    // snake_case rename: the default variant serializes to that string.
    assert_eq!(
        serde_json::to_string(&FileAccess::WorktreePlusCaches).unwrap(),
        r#""worktree_plus_caches""#
    );
    let f: FileAccess = serde_json::from_str(r#""host""#).unwrap();
    assert_eq!(f, FileAccess::Host);
}

#[test]
fn sandbox_limits_default_and_parse() {
    let d = SandboxLimits::default();
    assert!(d.cpu.is_none() && d.memory.is_none());
    let cfg: Config = toml::from_str("[sandbox.limits]\ncpu = \"2\"\nmemory = \"4G\"\n").unwrap();
    assert_eq!(cfg.sandbox.limits.cpu.as_deref(), Some("2"));
    assert_eq!(cfg.sandbox.limits.memory.as_deref(), Some("4G"));
}

#[test]
fn sandbox_warm_direnv_and_prepare_parse() {
    // Default: warm on, no prepare hooks.
    let d = SandboxConfig::default();
    assert_eq!(d.warm_direnv, WarmDirenv::Auto);
    assert!(d.prepare.is_empty());
    // Round-trips from a `[sandbox]` table, and the overlay layers them.
    let cfg: Config = toml::from_str(
        "[sandbox]\nwarm_direnv = \"allowed-only\"\nprepare = [\"mise install\", \"echo hi\"]\n",
    )
    .unwrap();
    assert_eq!(cfg.sandbox.warm_direnv, WarmDirenv::AllowedOnly);
    assert_eq!(cfg.sandbox.prepare, vec!["mise install", "echo hi"]);
    // Unknown value warns and falls back to the default (infallible enum).
    let cfg2: Config = toml::from_str("[sandbox]\nwarm_direnv = \"bogus\"\n").unwrap();
    assert_eq!(cfg2.sandbox.warm_direnv, WarmDirenv::Auto);
    // `off` aliases.
    assert_eq!(
        WarmDirenv::from_str_validated("off").unwrap(),
        WarmDirenv::Off
    );
    assert_eq!(
        WarmDirenv::from_str_validated("false").unwrap(),
        WarmDirenv::Off
    );
}

// ---- launch_spec full env coverage ----

#[test]
fn llm_proxy_launch_spec_sets_all_stream_env() {
    let cfg = LlmProxyConfig {
        enabled: true,
        listen: "0.0.0.0:9000".into(),
        config_path: String::new(),
        routing: RoutingStrategy::Speculative,
        first_byte_timeout_secs: 7,
        idle_timeout_secs: 99,
        heartbeat_secs: 3,
        token_reduction: true,
        token_reduction_level: CompressionLevel::Aggressive,
        ..Default::default()
    };
    let (prog, _args, env) = cfg.launch_spec().unwrap();
    assert_eq!(prog, "szproxy");
    assert_eq!(env.get("SZPROXY_LISTEN").unwrap(), "0.0.0.0:9000");
    assert_eq!(env.get("SZPROXY_FIRST_BYTE_TIMEOUT").unwrap(), "7");
    assert_eq!(env.get("SZPROXY_STREAM_IDLE_TIMEOUT").unwrap(), "99");
    assert_eq!(env.get("SZPROXY_STREAM_HEARTBEAT_INTERVAL").unwrap(), "3");
    assert_eq!(env.get("SZPROXY_COMPRESS").unwrap(), "1");
    assert_eq!(env.get("SZPROXY_COMPRESS_LEVEL").unwrap(), "aggressive");
    assert_eq!(env.get("SZPROXY_ROUTING").unwrap(), "speculative");
    // token_reduction off → SZPROXY_COMPRESS = "0".
    let off = LlmProxyConfig {
        enabled: true,
        token_reduction: false,
        ..Default::default()
    };
    let (_, _, env) = off.launch_spec().unwrap();
    assert_eq!(env.get("SZPROXY_COMPRESS").unwrap(), "0");
}

// ---- AppsConfig::effective_tab_order / normalized_default_tab edges ----

#[test]
fn effective_tab_order_dedups_and_appends_missing() {
    let a = AppsConfig {
        // duplicates, unknown ids, and a whitespace-padded built-in.
        default_tab: "work".into(),
        tab_order: vec![
            "bogus".into(),
            "comms".into(),
            " work ".into(),
            "work".into(),
        ],
    };
    // unknown ids dropped, trimmed, deduped; the only built-in is `work`.
    assert_eq!(a.effective_tab_order(), vec!["work"]);
}

#[test]
fn effective_tab_order_empty_falls_back_to_builtins() {
    let a = AppsConfig {
        default_tab: "work".into(),
        tab_order: Vec::new(),
    };
    assert_eq!(a.effective_tab_order(), vec!["work"]);
}

#[test]
fn normalized_default_tab_present_and_falls_back_to_first() {
    let present = AppsConfig {
        default_tab: " work ".into(),
        tab_order: vec!["work".into()],
    };
    assert_eq!(present.normalized_default_tab(), "work");
    // Unknown default → first of the effective order (`work`).
    let bad = AppsConfig {
        default_tab: "nonexistent".into(),
        tab_order: vec!["comms".into(), "work".into()],
    };
    assert_eq!(bad.normalized_default_tab(), "work");
}

// ---- ConfigOverlay::apply field-by-field ----

#[test]
fn config_overlay_apply_sets_every_field() {
    let overlay = ConfigOverlay {
        worktrees_dir: Some("/wt".into()),
        workspaces_dir: Some("/ws".into()),
        base_branch: Some("main".into()),
        window_margin: Some(1),
        branch_prefix: Some("pfx/".into()),
        picker: Some(Picker::Fzf),
        worktree_mode: Some(WorktreeMode::InRepo),
        name_scheme: Some(NameScheme::Numbered),
        auto_remove_worktree: Some(true),
        repo_scan_depth: Some(7),
        profile: Some("vim".into()),
        accent: Some("#111111".into()),
        focus_border: Some("#222222".into()),
        frame_border: Some("#333333".into()),
        theme_color: Some(ColorMode::Ansi256),
        theme_glyphs: Some(GlyphMode::Unicode),
        theme_agent_glyphs: Some(AgentGlyphs::Auto),
        pr_ttl_secs: Some(99),
        watch_pr_interval_secs: Some(43),
        metrics_interval_secs: Some(11.0),
        metrics_timeout_ms: Some(1234),
        metrics_max_body_bytes: Some(4096),
        apps_default_tab: Some("chat".into()),
        apps_tab_order: Some(vec!["chat".into(), "work".into()]),
        log_level: Some(LogLevel::Debug),
        log_file: Some(true),
        log_dir: Some("/logs".into()),
        log_rotation_size_mb: Some(12),
        log_max_files: Some(3),
        log_format: Some(LogFormat::Json),
        disk_show_sizes: Some(false),
        disk_warn_threshold_gb: Some(250),
        disk_scan_interval_secs: Some(90),
        disk_auto_clean_on_merge: Some(false),
        disk_clean_on_pr_closed: Some(true),
        disk_sccache: Some(true),
        disk_sccache_dir: Some("/cache/sccache".into()),
        disk_shared_target_dir: Some("/cache/target".into()),
        sandbox: SandboxOverlay {
            enabled: Some(false),
            ..Default::default()
        },
    };
    let mut cfg = Config::default();
    overlay.apply(&mut cfg);
    assert_eq!(cfg.worktrees_dir, "/wt");
    assert_eq!(cfg.workspaces_dir, "/ws");
    assert_eq!(cfg.base_branch, "main");
    assert_eq!(cfg.window_margin, 1);
    assert_eq!(cfg.branch_prefix, "pfx/");
    assert_eq!(cfg.picker, Picker::Fzf);
    assert_eq!(cfg.worktree_mode, WorktreeMode::InRepo);
    assert_eq!(cfg.name_scheme, NameScheme::Numbered);
    assert!(cfg.auto_remove_worktree);
    assert_eq!(cfg.repo_scan_depth, 7);
    assert_eq!(cfg.profile, "vim");
    assert_eq!(cfg.theme.accent, "#111111");
    assert_eq!(cfg.theme.focus_border, "#222222");
    assert_eq!(cfg.theme.colors.border.as_deref(), Some("#333333"));
    assert_eq!(cfg.theme.agent_glyphs, AgentGlyphs::Auto);
    assert_eq!(cfg.pr.ttl_secs, 99);
    assert_eq!(cfg.watch.pr_interval_secs, 43);
    assert_eq!(cfg.metrics.interval_secs, 11.0);
    assert_eq!(cfg.metrics.timeout_ms, 1234);
    assert_eq!(cfg.metrics.max_body_bytes, 4096);
    assert_eq!(cfg.apps.default_tab, "chat");
    assert_eq!(cfg.apps.tab_order, vec!["chat", "work"]);
    assert_eq!(cfg.log.level, LogLevel::Debug);
    assert!(cfg.log.file);
    assert_eq!(cfg.log.dir, "/logs");
    assert_eq!(cfg.log.rotation_size_mb, 12);
    assert_eq!(cfg.log.max_files, 3);
    assert_eq!(cfg.log.format, LogFormat::Json);
    assert!(!cfg.disk.show_sizes);
    assert_eq!(cfg.disk.warn_threshold_gb, 250);
    assert_eq!(cfg.disk.scan_interval_secs, 90);
    assert!(!cfg.disk.auto_clean_on_merge);
    assert!(cfg.disk.clean_on_pr_closed);
    assert!(cfg.disk.sccache);
    assert_eq!(cfg.disk.sccache_dir, "/cache/sccache");
    assert_eq!(cfg.disk.shared_target_dir, "/cache/target");
    assert!(!cfg.sandbox.enabled);
}

#[test]
fn config_overlay_empty_leaves_base_untouched() {
    let mut cfg = Config::default();
    let before = cfg.clone();
    ConfigOverlay::default().apply(&mut cfg);
    // Spot-check a few fields are unchanged.
    assert_eq!(cfg.branch_prefix, before.branch_prefix);
    assert_eq!(cfg.picker, before.picker);
    assert_eq!(cfg.sandbox.enabled, before.sandbox.enabled);
    // An empty sandbox overlay must not be applied.
    assert!(ConfigOverlay::default().sandbox.is_empty());
}

// ---- SandboxOverlay::apply remaining branches + is_empty ----

#[test]
fn sandbox_overlay_apply_covers_remaining_fields() {
    let mut base = SandboxConfig::default();
    let overlay = SandboxOverlay {
        default_backend: Some(SandboxBackend::Docker),
        file_access: Some(FileAccess::All),
        ports: Some(vec!["8080:8080".into()]),
        gpu: Some("all".into()),
        limits: Some(SandboxLimits {
            cpu: Some("2".into()),
            memory: Some("8G".into()),
        }),
        volumes: Some(std::collections::HashMap::from([(
            "vol".to_string(),
            "/data".to_string(),
        )])),
        compose: Some("docker-compose.yml".into()),
        auto_caches: Some(false),
        shell: Some("zsh".into()),
        network_audit: Some(true),
        ..Default::default()
    };
    overlay.apply(&mut base);
    assert_eq!(base.default_backend, SandboxBackend::Docker);
    assert_eq!(base.file_access, FileAccess::All);
    assert_eq!(base.ports, vec!["8080:8080"]);
    assert_eq!(base.gpu.as_deref(), Some("all"));
    assert_eq!(base.limits.cpu.as_deref(), Some("2"));
    assert_eq!(base.limits.memory.as_deref(), Some("8G"));
    assert_eq!(base.volumes.get("vol").map(String::as_str), Some("/data"));
    assert_eq!(base.compose.as_deref(), Some("docker-compose.yml"));
    assert!(!base.auto_caches);
    assert_eq!(base.shell, "zsh");
    assert!(base.network_audit);
}

#[test]
fn sandbox_overlay_is_empty_detects_any_set_field() {
    assert!(SandboxOverlay::default().is_empty());
    // Each is_empty()-tracked field flips it to non-empty.
    let with_allow = SandboxOverlay {
        network_allow: Some(vec!["x".into()]),
        ..Default::default()
    };
    assert!(!with_allow.is_empty());
    let with_remote = SandboxOverlay {
        remote: Some(RemoteOverlay::default()),
        ..Default::default()
    };
    assert!(!with_remote.is_empty());
    let with_backend = SandboxOverlay {
        backend: Some(SandboxBackend::Docker),
        ..Default::default()
    };
    assert!(!with_backend.is_empty());
}

#[test]
fn remote_overlay_apply_sets_each_field() {
    let mut base = RemoteConfig::default();
    RemoteOverlay {
        host: Some("h".into()),
        port: Some(2022),
        transport: Some(RemoteTransport::Ssh),
        mode: Some(RemoteMode::LocalExec),
        remote_dir: Some("/srv".into()),
        forward_agent: Some(false),
    }
    .apply(&mut base);
    assert_eq!(base.host, "h");
    assert_eq!(base.port, 2022);
    assert_eq!(base.transport, RemoteTransport::Ssh);
    assert_eq!(base.mode, RemoteMode::LocalExec);
    assert_eq!(base.remote_dir, "/srv");
    assert!(!base.forward_agent);
}

// ---- env_overlay: remaining branches ----

#[test]
fn env_overlay_apps_tab_order_parses_csv() {
    let env = map_env(&[("SUPERZEJ_APPS_TAB_ORDER", " work , foo ,, bar ")]);
    let o = env_overlay(&env);
    // parse_list trims and drops empties (validity filtering happens later
    // in effective_tab_order).
    assert_eq!(
        o.apps_tab_order,
        Some(vec![
            "work".to_string(),
            "foo".to_string(),
            "bar".to_string()
        ])
    );
}

#[test]
fn env_overlay_metrics_rejects_non_finite_floats() {
    let env = map_env(&[("SUPERZEJ_METRICS_INTERVAL_SECS", "inf")]);
    assert_eq!(env_overlay(&env).metrics_interval_secs, None);
    let env = map_env(&[("SUPERZEJ_METRICS_INTERVAL_SECS", "abc")]);
    assert_eq!(env_overlay(&env).metrics_interval_secs, None);
}

#[test]
fn env_overlay_bad_enum_values_yield_none() {
    let env = map_env(&[
        ("SUPERZEJ_WORKTREE_MODE", "bogus"),
        ("SUPERZEJ_NAME_SCHEME", "bogus"),
        ("SUPERZEJ_LOG_LEVEL", "bogus"),
        ("SUPERZEJ_LOG_FORMAT", "bogus"),
        ("SUPERZEJ_SANDBOX_BACKEND", "bogus"),
        ("SUPERZEJ_SANDBOX_NETWORK", "bogus"),
        ("SUPERZEJ_SANDBOX_ON_MISSING", "bogus"),
    ]);
    let o = env_overlay(&env);
    assert_eq!(o.worktree_mode, None);
    assert_eq!(o.name_scheme, None);
    assert_eq!(o.log_level, None);
    assert_eq!(o.log_format, None);
    assert_eq!(o.sandbox.backend, None);
    assert_eq!(o.sandbox.network, None);
    assert_eq!(o.sandbox.on_missing, None);
}

#[test]
fn env_overlay_log_and_metrics_bad_numbers_skip() {
    let env = map_env(&[
        ("SUPERZEJ_LOG_ROTATION_SIZE_MB", "huge"),
        ("SUPERZEJ_LOG_MAX_FILES", "lots"),
        ("SUPERZEJ_METRICS_TIMEOUT_MS", "soon"),
        ("SUPERZEJ_METRICS_MAX_BODY_BYTES", "big"),
        ("SUPERZEJ_WATCH_PR_INTERVAL", "later"),
    ]);
    let o = env_overlay(&env);
    assert_eq!(o.log_rotation_size_mb, None);
    assert_eq!(o.log_max_files, None);
    assert_eq!(o.metrics_timeout_ms, None);
    assert_eq!(o.metrics_max_body_bytes, None);
    assert_eq!(o.watch_pr_interval_secs, None);
}

// ---- get_dotted: theme.preset/pane_padding/undercurl/hues + log.dir ----

#[test]
fn get_dotted_theme_preset_padding_undercurl_and_hues() {
    let mut c = Config::default();
    c.theme.preset = "storm".into();
    c.theme.pane_padding = 3;
    c.theme.undercurl = UndercurlMode::On;
    c.theme.hues.teal = Some("#0a0b0c".into());
    assert_eq!(c.get_dotted("theme.preset").as_deref(), Some("storm"));
    assert_eq!(c.get_dotted("theme.pane_padding").as_deref(), Some("3"));
    assert_eq!(c.get_dotted("theme.undercurl").as_deref(), Some("on"));
    assert_eq!(c.get_dotted("theme.hues.teal").as_deref(), Some("#0a0b0c"));
    // Unset hue → empty string; unknown hue → None.
    assert_eq!(c.get_dotted("theme.hues.red").as_deref(), Some(""));
    assert_eq!(c.get_dotted("theme.hues.bogus"), None);
    // confirm_delete + remote sub-keys.
    assert_eq!(c.get_dotted("confirm_delete").as_deref(), Some("true"));
    assert_eq!(
        c.get_dotted("sandbox.remote.transport").as_deref(),
        Some("mosh")
    );
    assert_eq!(
        c.get_dotted("sandbox.remote.mode").as_deref(),
        Some("remote")
    );
    // log.dir resolves to the default path (no tilde).
    let dir = c.get_dotted("log.dir").unwrap();
    assert!(dir.ends_with("superzej/logs"), "{dir}");
}

// ---- post_process behavior ----

#[test]
fn post_process_populates_default_agents_and_tools() {
    // Point at a non-existent file so the file layer is empty and defaults
    // (not the host's real ~/.config) drive post_process.
    let dir = tmpdir("ppdefaults");
    let f = dir.join("missing.toml");
    let c = Config::load_layered(&MapEnv::default(), &[], Some(f));
    assert!(c.agents.iter().any(|a| a.name == "claude"));
    assert!(c.agents.iter().any(|a| a.name == "shell"));
    assert!(c.tools.iter().any(|t| t.name == "lazygit"));
    assert!(c.tools.iter().any(|t| t.name == "editor"));
    // repo_roots defaults to [workspaces_dir] when unset.
    assert_eq!(c.repo_roots, vec![c.workspaces_dir.clone()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn post_process_expands_pin_cwd_tilde() {
    let dir = tmpdir("ppcwd");
    let f = dir.join("c.toml");
    std::fs::write(&f, "[[pins]]\nname='x'\ncommand='c'\ncwd='~/sub'\n").unwrap();
    let c = Config::load_layered(&MapEnv::default(), &[], Some(f));
    let cwd = c.pins[0].cwd.as_deref().unwrap();
    assert!(!cwd.starts_with('~'), "{cwd}");
    assert!(cwd.ends_with("/sub"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- apply_override_str: apps.tab_order shortcut + load_layered error path ----

#[test]
fn apply_override_str_apps_tab_order_splits_csv() {
    let mut cfg = Config::default();
    Config::apply_override_str(&mut cfg, "apps.tab_order", " work , foo ,, bar ").unwrap();
    assert_eq!(cfg.apps.tab_order, vec!["work", "foo", "bar"]);
}

#[test]
fn load_layered_recovers_on_parse_error_and_still_applies_layers() {
    // A malformed file forces the load_layered error branch, which rebuilds
    // from defaults and re-applies env + flags.
    let dir = tmpdir("recover");
    let f = dir.join("c.toml");
    std::fs::write(&f, "= = broken\n").unwrap();
    let env = map_env(&[("SUPERZEJ_BRANCH_PREFIX", "env/")]);
    let flags = vec!["picker=fzf".to_string()];
    let c = Config::load_layered(&env, &flags, Some(f));
    assert_eq!(c.branch_prefix, "env/");
    assert_eq!(c.picker, Picker::Fzf);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_agent_env_routes_through_proxy_when_configured() {
    // Off by default (no route_agent → no injection).
    assert!(LlmProxyConfig::default().remote_agent_env(None).is_empty());
    // `route_agent` alone is the single switch: an empty remote_base_url resolves
    // to the auto reverse-tunnel loopback, so the pi (`SUPERZEJ_PROXY_*`) env IS
    // injected and the tunnel port is signalled — but NOT `ANTHROPIC_BASE_URL`
    // (claude talks to Anthropic directly unless `route_claude`).
    let only_route = LlmProxyConfig {
        route_agent: true,
        ..Default::default()
    };
    let oenv = only_route.remote_agent_env(None);
    assert!(
        oenv.iter()
            .any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL" && v == "http://127.0.0.1:8383"),
        "route_agent alone → pi proxy vars injected at the auto loopback"
    );
    assert!(
        !oenv.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"),
        "claude is NOT routed by default (route_claude off)"
    );
    assert_eq!(only_route.remote_tunnel_port(), Some(8383));
    // Configured, route_claude ON → additionally inject the ANTHROPIC_* vars.
    let lp = LlmProxyConfig {
        route_agent: true,
        route_claude: true,
        remote_base_url: "https://proxy.example".into(),
        ..Default::default()
    };
    let env = lp.remote_agent_env(Some("vk-1"));
    assert!(
        env.iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://proxy.example"),
        "route_claude → claude code / SDK honor ANTHROPIC_BASE_URL"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "vk-1")
    );
    assert!(env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_BASE_URL"));
    assert!(
        env.iter()
            .any(|(k, v)| k == "SUPERZEJ_PROXY_KEY" && v == "vk-1"),
        "pi always gets the virtual key regardless of route_claude"
    );
    assert_eq!(
        lp.remote_tunnel_port(),
        None,
        "explicit URL needs no tunnel"
    );
    // route_claude OFF with a virtual key: pi key present, ANTHROPIC_* absent.
    let keyed_no_claude = LlmProxyConfig {
        route_agent: true,
        remote_base_url: "https://proxy.example".into(),
        ..Default::default()
    };
    let kenv = keyed_no_claude.remote_agent_env(Some("vk-2"));
    assert!(
        kenv.iter()
            .any(|(k, v)| k == "SUPERZEJ_PROXY_KEY" && v == "vk-2")
    );
    assert!(!kenv.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
    assert!(!kenv.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));

    // "auto" → derive the in-sandbox tunnel URL from the proxy port + signal
    // the host to stand a reverse tunnel up on that port (pi still needs it).
    let auto = LlmProxyConfig {
        route_agent: true,
        route_claude: true,
        remote_base_url: "auto".into(),
        listen: "127.0.0.1:9999".into(),
        ..Default::default()
    };
    assert_eq!(
        auto.remote_base_url().as_deref(),
        Some("http://127.0.0.1:9999")
    );
    assert_eq!(auto.remote_tunnel_port(), Some(9999));
    let aenv = auto.remote_agent_env(None);
    assert!(
        aenv.iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "http://127.0.0.1:9999")
    );
}

#[test]
fn local_agent_env_targets_host_loopback_regardless_of_remote_base_url() {
    // Off by default.
    assert!(LlmProxyConfig::default().local_agent_env().is_empty());
    // `route_agent` on: always the LOCAL listen loopback, even when
    // `remote_base_url` points at an external endpoint for remote sandboxes.
    let lp = LlmProxyConfig {
        route_agent: true,
        route_claude: true,
        remote_base_url: "https://proxy.example.ts.net".into(),
        listen: "127.0.0.1:8383".into(),
        ..Default::default()
    };
    let env = lp.local_agent_env();
    let url = "http://127.0.0.1:8383";
    assert!(
        env.iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == url),
        "route_claude → host agent uses the local loopback, not the external remote URL"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL" && v == url),
        "the pi extension's base URL is the local proxy"
    );
    assert!(env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_API"));
    assert!(env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_MODEL"));
    // Keyless (like the sprite path) — the pi extension falls back to default.
    assert!(!env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_KEY"));
    assert!(!env.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
    // Default (route_claude off): pi vars only, claude talks upstream directly.
    let no_claude = LlmProxyConfig {
        route_agent: true,
        listen: "127.0.0.1:8383".into(),
        ..Default::default()
    };
    let nenv = no_claude.local_agent_env();
    assert!(nenv.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_BASE_URL"));
    assert!(
        !nenv.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"),
        "claude not routed on the host by default"
    );
    // Honors a custom listen port.
    let custom = LlmProxyConfig {
        route_agent: true,
        listen: "127.0.0.1:7000".into(),
        ..Default::default()
    };
    assert!(
        custom
            .local_agent_env()
            .iter()
            .any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL" && v == "http://127.0.0.1:7000")
    );
}

#[test]
fn passthrough_env_remote_drops_host_local_socket_vars() {
    let sb = SandboxConfig {
        env_passthrough: vec!["SZ_TEST_TOK_42".into(), "SSH_AUTH_SOCK".into()],
        ..Default::default()
    };
    // SAFETY: test-local env mutation with a unique key; restored below.
    unsafe {
        std::env::set_var("SZ_TEST_TOK_42", "secret");
        std::env::set_var("SSH_AUTH_SOCK", "/tmp/agent.sock");
    }
    let remote = sb.passthrough_env_remote();
    assert!(
        remote.iter().any(|(k, _)| k == "SZ_TEST_TOK_42"),
        "value secrets pass to a remote placement"
    );
    assert!(
        !remote.iter().any(|(k, _)| k == "SSH_AUTH_SOCK"),
        "host-local socket vars are dropped for a remote placement"
    );
    // The unfiltered passthrough still carries it (OCI bind-mount case).
    assert!(
        sb.passthrough_env()
            .iter()
            .any(|(k, _)| k == "SSH_AUTH_SOCK")
    );
    unsafe {
        std::env::remove_var("SZ_TEST_TOK_42");
        std::env::remove_var("SSH_AUTH_SOCK");
    }
}

#[test]
fn remote_safe_term_downgrades_exotic_terminals() {
    // Exotic types the remote won't have terminfo for → xterm-256color.
    assert_eq!(remote_safe_term("xterm-ghostty"), "xterm-256color");
    assert_eq!(remote_safe_term("xterm-kitty"), "xterm-256color");
    assert_eq!(remote_safe_term("alacritty"), "xterm-256color");
    assert_eq!(remote_safe_term(""), "xterm-256color");
    // Universally-shipped types pass through unchanged.
    assert_eq!(remote_safe_term("xterm-256color"), "xterm-256color");
    assert_eq!(remote_safe_term("screen-256color"), "screen-256color");
    assert_eq!(remote_safe_term("vt100"), "vt100");
}

#[test]
fn passthrough_env_remote_injects_devshell_selector() {
    // Unset → no SUPERZEJ_DEVSHELL (host default shell unchanged).
    let plain = SandboxConfig::default();
    assert!(
        !plain
            .passthrough_env_remote()
            .iter()
            .any(|(k, _)| k == "SUPERZEJ_DEVSHELL"),
        "no selector when [sandbox] devshell is unset"
    );
    // Set → exported so the sandbox `.envrc` enters that attr.
    let sb = SandboxConfig {
        devshell: "sandbox".into(),
        ..Default::default()
    };
    assert!(
        sb.passthrough_env_remote()
            .iter()
            .any(|(k, v)| k == "SUPERZEJ_DEVSHELL" && v == "sandbox"),
        "devshell attr exported as SUPERZEJ_DEVSHELL"
    );
}

#[test]
fn passthrough_env_remote_normalizes_term() {
    let sb = SandboxConfig {
        env_passthrough: vec!["TERM".into()],
        ..Default::default()
    };
    // SAFETY: test-local env mutation; restored below.
    unsafe {
        std::env::set_var("TERM", "xterm-ghostty");
    }
    let remote = sb.passthrough_env_remote();
    assert!(
        remote
            .iter()
            .any(|(k, v)| k == "TERM" && v == "xterm-256color"),
        "exotic host TERM normalized for the remote: {remote:?}"
    );
    unsafe {
        std::env::remove_var("TERM");
    }
}

#[test]
fn env_failover_resolves_override_then_global() {
    let dir = tmpdir("failover");
    let mut cfg = Config::default();
    // Global default is opt-out (halt+warn).
    assert!(!cfg.env_failover(&dir, "default"));
    // An env with no override inherits the (repo-overlaid) global.
    cfg.sandbox.failover = true;
    cfg.env.insert("inherit".into(), EnvConfig::default());
    assert!(cfg.env_failover(&dir, "inherit"));
    // Some(false) forces a halt even when the global allows failover.
    cfg.env.insert(
        "strict".into(),
        EnvConfig {
            failover: Some(false),
            ..Default::default()
        },
    );
    assert!(!cfg.env_failover(&dir, "strict"));
    // Some(true) allows failover even when the global forbids it.
    cfg.sandbox.failover = false;
    cfg.env.insert(
        "loose".into(),
        EnvConfig {
            failover: Some(true),
            ..Default::default()
        },
    );
    assert!(cfg.env_failover(&dir, "loose"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- repo overlay parse-error and yaml/json error paths ----

#[test]
fn repo_overlay_malformed_file_is_ignored() {
    let dir = tmpdir("badoverlay");
    std::fs::write(dir.join(".superzej.toml"), "[sandbox\nbroken = =\n").unwrap();
    let cfg = Config::default();
    // Malformed overlay warns and is ignored → global sandbox survives.
    let sb = cfg.repo_sandbox(&dir);
    assert!(sb.enabled);
    assert_eq!(sb.image, "");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repo_overlay_parse_error_surfaces_dropped_env_selection() {
    let dir = tmpdir("parseerr");
    // The real-world footgun: `env = "sprites"` (selector string) colliding
    // with an `[env.sprites.provider]` table — fails to parse, and the dropped
    // overlay would silently lose the sprites selection.
    std::fs::write(
        dir.join(".superzej.toml"),
        "env = \"sprites\"\n[env.sprites.provider]\nconnect = \"ssh\"\n",
    )
    .unwrap();
    let pe = repo_overlay_parse_error(&dir).expect("a present, malformed overlay");
    assert!(!pe.error.is_empty());
    assert_eq!(pe.selected_env, "sprites", "lenient env selector recovered");
    // A clean overlay yields None.
    std::fs::write(dir.join(".superzej.toml"), "env = \"sprites\"\n").unwrap();
    assert!(
        repo_overlay_parse_error(&dir).is_none(),
        "valid overlay parses"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lenient_env_selector_reads_toml_and_yaml_not_tables() {
    assert_eq!(lenient_env_selector("env = \"sprites\"\n"), "sprites");
    assert_eq!(lenient_env_selector("env: bigbox\n"), "bigbox");
    // Must not match table headers or sibling keys.
    assert_eq!(
        lenient_env_selector("[env.sprites]\nprovider = \"sprites\"\n"),
        ""
    );
    assert_eq!(
        lenient_env_selector("env_name = \"x\"\nenvironment = \"y\"\n"),
        ""
    );
}

#[test]
fn repo_overlay_json_format_loads() {
    let dir = tmpdir("jsonoverlay");
    std::fs::write(
        dir.join(".superzej.json"),
        r#"{"sandbox":{"backend":"docker","ports":["1:1"],"file_access":"all"}}"#,
    )
    .unwrap();
    // JSON parses, then clamps: backend forbidden, file_access=all denied
    // (widening), ports gated (surfaced, not applied).
    let cfg = Config::default();
    let r = cfg.repo_sandbox_resolved(&dir, &crate::config_resolve::Approvals::deny_all());
    assert_eq!(r.sandbox.backend, SandboxBackend::Auto, "backend denied");
    assert_eq!(r.sandbox.file_access, cfg.sandbox.file_access, "all denied");
    assert!(r.sandbox.ports.is_empty(), "ports gated, not applied");
    assert!(r.pending.iter().any(|p| p.key == "sandbox.ports"));
    assert!(r.events.iter().any(|e| e.key == "sandbox.backend"));
    assert!(r.events.iter().any(|e| e.key == "sandbox.file_access"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keybind_config_iter_and_into_iter_and_get() {
    let mut kb = KeybindConfig::default();
    assert!(kb.is_empty());
    assert!(kb.insert("a".into(), "X".into()).is_none());
    // insert returns the previous value when replacing.
    assert_eq!(kb.insert("a".into(), "Y".into()).as_deref(), Some("X"));
    assert_eq!(kb.get("a").map(String::as_str), Some("Y"));
    assert!(!kb.is_empty());
    let collected: Vec<_> = kb.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(collected, vec![("a".to_string(), "Y".to_string())]);
    // IntoIterator for &KeybindConfig.
    let via_ref: Vec<_> = (&kb).into_iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(via_ref, vec!["a".to_string()]);
}

#[test]
fn config_path_uses_xdg_config_home() {
    assert!(Config::path().ends_with("superzej/config.toml"));
}

#[test]
fn validate_str_non_table_top_level_is_tolerated() {
    // A document whose root toml::Value is not a table hits the
    // `as_table() == None` early-return (empty error list, no panic). A bare
    // scalar isn't valid top-level TOML, so wrap it in an array value via a
    // key to keep parsing while still exercising the guard indirectly.
    // (An empty document is the simplest non-erroring non-keyed input.)
    assert!(validate_str("   ").is_empty());
    assert!(validate_str("\n# comment only\n").is_empty());
}

#[test]
fn lsp_servers_and_actions_and_custom_actions_parse() {
    let cfg: Config = toml::from_str(
        r#"
[lsp]
enabled = false
hover = false
[[lsp.servers]]
lang = "rust"
command = "rust-analyzer"
args = ["--stdio"]

[[actions]]
name = "open-logs"
key = "Alt L"
run = "journalctl -f"
menu = true
hint = "logs"

[[actions]]
name = "bare"
key = "Alt B"
run = "echo hi"
"#,
    )
    .unwrap();
    assert!(!cfg.lsp.enabled);
    assert!(!cfg.lsp.hover);
    assert_eq!(cfg.lsp.servers[0].lang, "rust");
    assert_eq!(cfg.lsp.servers[0].args, vec!["--stdio"]);
    assert_eq!(cfg.actions.len(), 2);
    let a = &cfg.actions[0];
    assert!(a.menu);
    assert_eq!(a.hint.as_deref(), Some("logs"));
    assert!(a.floating); // default_true
    assert!(a.close_on_exit); // default_true
    // bare action keeps menu=false and the default_true flags.
    let b = &cfg.actions[1];
    assert!(!b.menu);
    assert!(b.floating && b.close_on_exit);
    // run-form leaves the composite fields empty.
    assert_eq!(a.run.as_deref(), Some("journalctl -f"));
    assert!(a.action.is_none() && a.params.is_empty());
}

#[test]
fn composite_action_with_params_parses() {
    let cfg: Config = toml::from_str(
        r#"
[[actions]]
name = "scratch-shell"
key = "Alt N"
action = "new-worktree"
params = { sandbox = "bwrap", agent = "shell" }
menu = true

[[actions]]
name = "logs-pane"
key = "Alt L"
action = "new-pane"
params = { run = "tail -f log/dev.log", placement = "right" }
"#,
    )
    .unwrap();
    assert_eq!(cfg.actions.len(), 2);
    let a = &cfg.actions[0];
    assert!(a.run.is_none());
    assert_eq!(a.action.as_deref(), Some("new-worktree"));
    assert_eq!(a.params.get("sandbox").map(String::as_str), Some("bwrap"));
    assert_eq!(a.params.get("agent").map(String::as_str), Some("shell"));
    assert!(a.menu);
    let b = &cfg.actions[1];
    assert_eq!(b.action.as_deref(), Some("new-pane"));
    assert_eq!(b.params.get("placement").map(String::as_str), Some("right"));
}

#[test]
fn program_keybinds_and_remap_and_workspace_tables_parse() {
    let cfg: Config = toml::from_str(
        r#"
[program_keybinds.lazygit]
quit = "Ctrl q"
[program_remap.nvim]
"Alt j" = "j"
[workspace.myrepo.keybinds]
focus-down = "Alt j"
"#,
    )
    .unwrap();
    assert_eq!(
        cfg.program_keybinds
            .get("lazygit")
            .and_then(|k| k.get("quit"))
            .map(String::as_str),
        Some("Ctrl q")
    );
    assert_eq!(
        cfg.program_remap
            .get("nvim")
            .and_then(|m| m.get("Alt j"))
            .map(String::as_str),
        Some("j")
    );
    assert!(cfg.workspace.contains_key("myrepo"));
}

#[test]
fn named_command_with_hints_parses() {
    let cfg: Config = toml::from_str(
        r#"
[[tools]]
name = "lazygit"
command = "lazygit"
hints = [{ key = "q", label = "quit" }]
"#,
    )
    .unwrap();
    assert_eq!(cfg.tools[0].hints.len(), 1);
    assert_eq!(cfg.tools[0].hints[0].key, "q");
    assert_eq!(cfg.tools[0].hints[0].label, "quit");
}

#[test]
fn issues_full_table_parses() {
    let cfg: Config = toml::from_str(
        r#"
[issues]
provider = "linear"
ttl_secs = 120
max_issues = 50
filter_assignee_me = false
[issues.linear]
api_key = "env:LINEAR_API_KEY"
team_id = "TEAM"
[issues.jira]
base_url = "https://x.atlassian.net"
email = "me@x.com"
project_key = "PROJ"
[issues.github_issues]
extra_flags = ["--assignee", "@me"]
"#,
    )
    .unwrap();
    assert_eq!(cfg.issues.provider, IssueProviderKind::Linear);
    assert_eq!(cfg.issues.ttl_secs, 120);
    assert_eq!(cfg.issues.max_issues, 50);
    assert!(!cfg.issues.filter_assignee_me);
    assert_eq!(cfg.issues.linear.team_id, "TEAM");
    assert_eq!(cfg.issues.jira.project_key, "PROJ");
    assert_eq!(
        cfg.issues.github_issues.extra_flags,
        vec!["--assignee", "@me"]
    );
}

#[test]
fn llm_proxy_full_table_parses() {
    let cfg: Config = toml::from_str(
        r#"
[llm_proxy]
enabled = true
listen = "127.0.0.1:9999"
routing = "speculative"
refuse_on_breach = false
config_path = "/x.json"
first_byte_timeout_secs = 10
idle_timeout_secs = 20
heartbeat_secs = 5
token_reduction = true
token_reduction_level = "balanced"
route_agent = true
bouncer = true
"#,
    )
    .unwrap();
    assert!(cfg.llm_proxy.enabled);
    assert_eq!(cfg.llm_proxy.listen, "127.0.0.1:9999");
    assert_eq!(cfg.llm_proxy.routing, RoutingStrategy::Speculative);
    assert!(!cfg.llm_proxy.refuse_on_breach);
    assert_eq!(cfg.llm_proxy.first_byte_timeout_secs, 10);
    assert_eq!(cfg.llm_proxy.idle_timeout_secs, 20);
    assert_eq!(cfg.llm_proxy.heartbeat_secs, 5);
    assert!(cfg.llm_proxy.token_reduction);
    assert_eq!(
        cfg.llm_proxy.token_reduction_level,
        CompressionLevel::Balanced
    );
    assert!(cfg.llm_proxy.route_agent);
    assert!(cfg.llm_proxy.bouncer);
}

#[test]
fn llm_proxy_bouncer_off_by_default() {
    // The bouncer is opt-in: the additive integration (pi runs its own
    // tools in-process) stays the default.
    let cfg = LlmProxyConfig::default();
    assert!(!cfg.bouncer, "bouncer must default off — AI is additive");
    // A table that omits the key keeps the default.
    let parsed: Config = toml::from_str("[llm_proxy]\nenabled = true\n").unwrap();
    assert!(!parsed.llm_proxy.bouncer);
}

#[test]
fn metrics_interval_secs_alias_parses() {
    // serde alias: kebab-case keys are accepted.
    let cfg: Config = toml::from_str("[metrics]\ninterval-secs = 3.0\ntimeout-ms = 200\n").unwrap();
    assert_eq!(cfg.metrics.interval_secs, 3.0);
    assert_eq!(cfg.metrics.timeout_ms, 200);
}

#[test]
fn pin_start_and_restart_enums_parse() {
    let cfg: Config =
        toml::from_str("[[pins]]\nname='x'\ncommand='c'\nstart='eager'\nrestart='onfailure'\n")
            .unwrap();
    assert_eq!(cfg.pins[0].start, PinStart::Eager);
    assert_eq!(cfg.pins[0].restart, PinRestart::OnFailure);
    // Defaults.
    assert_eq!(PinStart::default(), PinStart::Lazy);
    assert_eq!(PinRestart::default(), PinRestart::Never);
}

#[test]
fn task_kind_default_and_parse() {
    assert_eq!(TaskKind::default(), TaskKind::Custom);
    let cfg: Config =
        toml::from_str("[[tasks]]\nname='b'\ncommand='make'\nkind='build'\n").unwrap();
    assert_eq!(cfg.tasks[0].kind, TaskKind::Build);
}

#[test]
fn worktree_template_default_is_all_empty() {
    let t = WorktreeTemplate::default();
    assert!(t.name.is_empty());
    assert!(t.base.is_none() && t.layout.is_none());
    assert!(t.pins.is_empty() && t.commands.is_empty());
}

#[test]
fn vpn_config_defaults_to_disabled() {
    let cfg = SandboxConfig::default();
    assert_eq!(cfg.vpn.provider, VpnProviderKind::None);
    assert!(!cfg.vpn.is_enabled());
    // Forward-looking defaults the runtime relies on.
    assert_eq!(cfg.vpn.mode, VpnMode::Sidecar);
    assert_eq!(cfg.vpn.on_error, VpnOnError::Fail);
    assert_eq!(cfg.vpn.dns, VpnDnsMode::Tunnel);
    assert!(cfg.vpn.ephemeral);
}

#[test]
fn vpn_provider_kind_aliases_and_default() {
    assert_eq!(VpnProviderKind::default(), VpnProviderKind::None);
    for (s, want) in [
        ("tailscale", VpnProviderKind::Tailscale),
        ("ts", VpnProviderKind::Tailscale),
        ("headscale", VpnProviderKind::Headscale),
        ("hs", VpnProviderKind::Headscale),
        ("wg-quick", VpnProviderKind::Wireguard),
        ("ovpn", VpnProviderKind::Openvpn),
        ("nb", VpnProviderKind::Netbird),
        ("zt", VpnProviderKind::Zerotier),
        ("command", VpnProviderKind::Custom),
        ("off", VpnProviderKind::None),
    ] {
        assert_eq!(VpnProviderKind::from_str_validated(s).unwrap(), want, "{s}");
    }
    // Unknown values warn and fall back to the default (infallible deser).
    let k: VpnProviderKind = serde_json::from_str(r#""bogus""#).unwrap();
    assert_eq!(k, VpnProviderKind::None);
}

#[test]
fn vpn_mode_and_dns_aliases() {
    assert_eq!(
        VpnMode::from_str_validated("in-container").unwrap(),
        VpnMode::InContainer
    );
    assert_eq!(
        VpnDnsMode::from_str_validated("filter_front").unwrap(),
        VpnDnsMode::FilterFront
    );
    assert_eq!(
        VpnOnError::from_str_validated("offline").unwrap(),
        VpnOnError::Offline
    );
}

#[test]
fn vpn_config_parses_from_toml_subtables() {
    let cfg: Config = toml::from_str(
        r#"
[sandbox.vpn]
provider = "headscale"
mode = "proxy"
dns = "filter-front"
ready_timeout_secs = 12
ephemeral = false

[sandbox.vpn.tailscale]
auth_key = "env:TS_AUTHKEY"
login_server = "https://headscale.example.com"
tags = ["tag:dev", "tag:ci"]
exit_node = "exit-1"
accept_routes = true
hostname = "my-node"

[sandbox.vpn.wireguard]
config_path = "/etc/wireguard/wg0.conf"
"#,
    )
    .unwrap();
    let v = &cfg.sandbox.vpn;
    assert_eq!(v.provider, VpnProviderKind::Headscale);
    assert_eq!(v.mode, VpnMode::Proxy);
    assert_eq!(v.dns, VpnDnsMode::FilterFront);
    assert_eq!(v.ready_timeout_secs, 12);
    assert!(!v.ephemeral);
    assert_eq!(v.tailscale.login_server, "https://headscale.example.com");
    assert_eq!(v.tailscale.tags, vec!["tag:dev", "tag:ci"]);
    assert_eq!(v.tailscale.exit_node, "exit-1");
    assert!(v.tailscale.accept_routes);
    assert_eq!(v.tailscale.hostname, "my-node");
    assert_eq!(v.wireguard.config_path, "/etc/wireguard/wg0.conf");
}

#[test]
fn vpn_config_round_trips_through_serialization() {
    let v = VpnConfig {
        provider: VpnProviderKind::Zerotier,
        zerotier: ZerotierConfig {
            network_id: "8056c2e21c000001".into(),
            ..ZerotierConfig::default()
        },
        ..VpnConfig::default()
    };
    let s = toml::to_string(&v).unwrap();
    let back: VpnConfig = toml::from_str(&s).unwrap();
    assert_eq!(v, back);
}

#[test]
fn sandbox_overlay_replaces_vpn_wholesale() {
    let mut base = SandboxConfig::default();
    base.vpn.provider = VpnProviderKind::Tailscale;
    base.vpn.tailscale.tags = vec!["tag:base".into()];

    let replacement = VpnConfig {
        provider: VpnProviderKind::Wireguard,
        ..VpnConfig::default()
    };
    let overlay = SandboxOverlay {
        vpn: Some(replacement),
        ..Default::default()
    };
    assert!(!overlay.is_empty());
    overlay.apply(&mut base);
    // Whole-table replace: the base's tailscale tags are gone, not merged.
    assert_eq!(base.vpn.provider, VpnProviderKind::Wireguard);
    assert!(base.vpn.tailscale.tags.is_empty());
}

#[test]
fn sandbox_profile_parses_sealed_tunnel() {
    for s in ["sealed-tunnel", "tunnel-only", "vpn-only"] {
        assert_eq!(
            SandboxProfile::from_str_validated(s).unwrap(),
            SandboxProfile::SealedTunnel,
            "{s}"
        );
    }
}

#[test]
fn sealed_tunnel_profile_floors_match_sealed_but_permits_vpn() {
    let st = SandboxProfile::SealedTunnel;
    // Same hardening floor as sealed.
    assert!(st.read_only_root());
    assert!(st.no_new_privileges());
    assert_eq!(st.pids_limit(), Some(256));
    assert_eq!(st.drop_capabilities(), vec!["ALL".to_string()]);
    assert!(st.forces_no_network());
    // ...but unlike plain sealed, it permits a tunnel.
    assert!(st.permits_vpn());
    assert!(!SandboxProfile::Sealed.permits_vpn());
    assert!(SandboxProfile::Hardened.permits_vpn());
    assert!(SandboxProfile::Open.permits_vpn());
}

#[test]
fn expand_env_ref_reads_file_prefix() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("sz-vpn-key-{}.txt", std::process::id()));
    std::fs::write(&path, "  super-secret-key\n").unwrap();
    let r = expand_env_ref(&format!("file:{}", path.display()));
    assert_eq!(r.as_deref(), Some("super-secret-key"));
    std::fs::remove_file(&path).unwrap();
    // Missing file -> None (not an error).
    assert_eq!(expand_env_ref("file:/no/such/sz/file"), None);
}
