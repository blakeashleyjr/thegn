use crate::config::{Config, SandboxProfile};
use crate::remote::GitLoc;
use crate::sandbox_pin::{
    EnvSelection, HOST_OWNED_REFUSAL, explain, parse_pin, refuse_inside_sandbox,
};

#[test]
fn sandbox_pin_aliases_use_the_config_enum_and_refuse_non_pinnable_profiles() {
    for alias in ["standard", "hardened", "guarded"] {
        assert_eq!(parse_pin(alias).unwrap(), SandboxProfile::Hardened);
    }
    for alias in ["sealed", "locked", "isolated"] {
        assert_eq!(parse_pin(alias).unwrap(), SandboxProfile::Sealed);
    }
    assert!(parse_pin("open").is_err());
    assert!(parse_pin("sealed-tunnel").is_err());
}

#[test]
fn sandbox_pin_resolution_honours_the_worktree_pin() {
    let cfg: Config =
        toml::from_str("[sandbox]\nprofile = 'sealed'\n[env.dev.sandbox]\nprofile = 'sealed'\n")
            .unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join(".thegn.toml"),
        "[sandbox]\nprofile = 'sealed'\n",
    )
    .unwrap();
    let loc = GitLoc::Local(repo.path().to_path_buf());
    let selection = EnvSelection::named("dev").with_profile(SandboxProfile::Hardened);
    let (env, _) = crate::config_resolve::resolve_environment(
        &cfg,
        repo.path(),
        &loc,
        repo.path(),
        &selection,
        &crate::config_resolve::Approvals::deny_all(),
    );
    assert_eq!(env.sandbox.profile, SandboxProfile::Hardened);
    assert_eq!(env.name, "dev");
    assert!(env.placement.is_local());
}

#[test]
fn sandbox_pin_explanations_match_profile_facts() {
    let standard = explain(SandboxProfile::Hardened);
    let sealed = explain(SandboxProfile::Sealed);
    assert!(
        standard
            .network
            .contains("Network and VPN attachments work")
            && !SandboxProfile::Hardened.forces_no_network()
    );
    assert!(sealed.network.contains("No network") && SandboxProfile::Sealed.forces_no_network());
    assert!(sealed.credentials.contains("hidden") && SandboxProfile::Sealed.hides_home());
    assert!(
        sealed.credentials.contains("SSH agent socket")
            && SandboxProfile::Sealed.seals_agent_socket()
    );
    assert!(
        sealed.one_liner.contains("all Linux capabilities")
            && !SandboxProfile::Sealed.drop_capabilities().is_empty()
    );
    assert!(
        standard.agents.contains("default capabilities")
            && SandboxProfile::Hardened.drop_capabilities().is_empty()
    );
}

#[test]
fn sandbox_pin_explanations_cover_profile_capability_and_network_facts() {
    let standard = explain(SandboxProfile::Hardened);
    let sealed = explain(SandboxProfile::Sealed);
    let standard_text = format!(
        "{} {} {} {} {} {}",
        standard.one_liner,
        standard.hides.join(" "),
        standard.network,
        standard.credentials,
        standard.agents,
        standard.extra.join(" ")
    );
    let sealed_text = format!(
        "{} {} {} {} {} {}",
        sealed.one_liner,
        sealed.hides.join(" "),
        sealed.network,
        sealed.credentials,
        sealed.agents,
        sealed.extra.join(" ")
    );
    assert_eq!(
        standard_text.to_ascii_lowercase().contains("network"),
        !SandboxProfile::Hardened.forces_no_network()
    );
    assert_eq!(
        sealed_text.to_ascii_lowercase().contains("network"),
        SandboxProfile::Sealed.forces_no_network()
    );
    assert!(standard_text.contains("readable"));
    assert!(standard_text.contains("~/.ssh"));
    assert!(standard_text.contains("default capabilities"));
    assert!(standard_text.contains("SSH") && standard_text.contains("provider"));
    assert!(standard_text.contains("VPN attachments work"));
    assert!(standard_text.contains("512 process limit"));
    assert!(
        standard_text
            .to_ascii_lowercase()
            .contains("root is read-only")
    );
    assert!(standard_text.contains("new privileges blocked"));
    assert!(sealed_text.contains("private") && sealed_text.contains("empty"));
    assert!(
        sealed_text.contains("~/.ssh") && sealed_text.to_ascii_lowercase().contains("credentials")
    );
    assert!(sealed_text.contains("SSH agent socket"));
    assert!(sealed_text.contains("API-backed agent CLIs"));
    assert!(sealed_text.contains("git fetch/push"));
    assert!(sealed_text.contains("offline builds"));
    assert!(sealed_text.contains("no network"));
    assert!(sealed_text.contains("VPN attachment"));
    assert!(sealed_text.contains("256 process limit"));
    assert!(
        sealed_text
            .to_ascii_lowercase()
            .contains("root is read-only")
    );
    assert!(sealed_text.contains("new privileges are blocked"));
    assert_eq!(
        sealed_text.contains("capabilit"),
        !SandboxProfile::Sealed.drop_capabilities().is_empty()
    );
    assert!(
        standard
            .how_to_change
            .contains("palette > Isolation profile")
    );
    assert!(standard.how_to_change.contains("the CLI"));
    assert_eq!(standard.how_to_change, sealed.how_to_change);
}

#[test]
fn sandbox_pin_absent_is_equivalent_across_precedence_inputs() {
    let cases = [
        ("", "", "", "", SandboxProfile::Hardened),
        (
            "[sandbox]\nprofile='sealed'",
            "",
            "",
            "",
            SandboxProfile::Sealed,
        ),
        (
            "[sandbox]\nprofile='standard'",
            "[env.dev.sandbox]\nprofile='sealed'",
            "",
            "dev",
            SandboxProfile::Sealed,
        ),
        (
            "[sandbox]\nprofile='standard'",
            "[env.dev.sandbox]\nprofile='standard'",
            "[sandbox]\nprofile='sealed'",
            "dev",
            SandboxProfile::Hardened,
        ),
        (
            "[sandbox]\nprofile='sealed'",
            "[env.dev.sandbox]\nprofile='standard'",
            "",
            "dev",
            SandboxProfile::Hardened,
        ),
        (
            "[sandbox]\nprofile='sealed'",
            "",
            "[sandbox]\nprofile='standard'",
            "",
            SandboxProfile::Sealed,
        ),
    ];
    for (global, env_overlay, repo_overlay, selected, expected_profile) in cases {
        let mut source = String::new();
        if !global.is_empty() {
            source.push_str(global);
            source.push('\n');
        }
        if !env_overlay.is_empty() {
            source.push_str(env_overlay);
            source.push('\n');
        }
        let cfg: Config = toml::from_str(&source).unwrap();
        let repo = tempfile::tempdir().unwrap();
        if !repo_overlay.is_empty() {
            std::fs::write(repo.path().join(".thegn.toml"), repo_overlay).unwrap();
        }
        let loc = GitLoc::Local(repo.path().to_path_buf());
        let name_only = if selected.is_empty() {
            EnvSelection::none()
        } else {
            EnvSelection::named(selected)
        };
        let wrapped = cfg.resolve_env(repo.path(), &loc, repo.path(), &name_only);
        assert_eq!(wrapped.sandbox.profile, expected_profile);
        assert_eq!(
            wrapped.name,
            if selected.is_empty() {
                "default"
            } else {
                selected
            },
            "the expected environment name is preserved"
        );
        assert!(wrapped.placement.is_local(), "these cases resolve locally");
        assert_eq!(wrapped.sandbox.backend, cfg.sandbox.default_backend);
        assert_eq!(wrapped.sandbox.network, cfg.sandbox.network);
    }
}

#[test]
fn sandbox_pin_overrides_global_env_and_repo_profiles_without_changing_placement() {
    let cfg: Config = toml::from_str(
        "[sandbox]\nprofile='standard'\n[env.dev]\nplacement='local'\n[env.dev.sandbox]\nprofile='sealed'\n",
    )
    .unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join(".thegn.toml"),
        "[sandbox]\nprofile='sealed'\n",
    )
    .unwrap();
    let loc = GitLoc::Local(repo.path().to_path_buf());
    let selection = EnvSelection::named("dev").with_profile(SandboxProfile::Hardened);
    let (resolved, _) = crate::config_resolve::resolve_environment(
        &cfg,
        repo.path(),
        &loc,
        repo.path(),
        &selection,
        &crate::config_resolve::Approvals::deny_all(),
    );
    assert_eq!(resolved.sandbox.profile, SandboxProfile::Hardened);
    assert_eq!(resolved.name, "dev");
    assert!(resolved.placement.is_local());
}

#[test]
fn sandbox_pin_does_not_lower_a_zone_profile_floor() {
    let cfg = Config::default();
    let repo = tempfile::tempdir().unwrap();
    let loc = GitLoc::Local(repo.path().to_path_buf());
    let selection = EnvSelection::none().with_profile(SandboxProfile::Hardened);
    let (mut resolved, _) = crate::config_resolve::resolve_environment(
        &cfg,
        repo.path(),
        &loc,
        repo.path(),
        &selection,
        &crate::config_resolve::Approvals::deny_all(),
    );
    crate::zone::apply_zone_ceilings(
        &mut resolved.sandbox,
        "restricted",
        &crate::zone::ZoneConfig {
            sandbox_floor: Some(SandboxProfile::Sealed),
            ..Default::default()
        },
    );
    assert_eq!(resolved.sandbox.profile, SandboxProfile::Sealed);
}

#[test]
fn sandbox_pin_host_owned_refusal_only_inside_a_sandbox() {
    assert!(refuse_inside_sandbox(false).is_ok());
    let error = refuse_inside_sandbox(true).unwrap_err();
    assert_eq!(error.to_string(), HOST_OWNED_REFUSAL);
}
