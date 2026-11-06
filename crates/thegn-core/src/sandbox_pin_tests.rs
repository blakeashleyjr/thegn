use crate::config::{Config, SandboxProfile};
use crate::env::GitLoc;
use crate::sandbox_pin::{EnvSelection, explain};

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
        standard.network.contains("Network works") && !SandboxProfile::Hardened.forces_no_network()
    );
    assert!(sealed.network.contains("No network") && SandboxProfile::Sealed.forces_no_network());
    assert!(sealed.credentials.contains("hidden") && SandboxProfile::Sealed.hides_home());
    assert!(
        sealed.credentials.contains("SSH agent socket")
            && SandboxProfile::Sealed.seals_agent_socket()
    );
    assert!(
        sealed.one_liner.contains("every capability")
            && !SandboxProfile::Sealed.drop_capabilities().is_empty()
    );
    assert!(
        standard.agents.contains("default capabilities")
            && SandboxProfile::Hardened.drop_capabilities().is_empty()
    );
}
