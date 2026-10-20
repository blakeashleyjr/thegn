use super::dotfiles_launch_gate;
use thegn_core::config::{Config, NamedCommand};

fn configured_agent() -> Config {
    Config {
        agents: vec![NamedCommand {
            name: "claude".into(),
            command: "claude".into(),
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
        }],
        ..Config::default()
    }
}

#[test]
fn dotfiles_failure_refuses_a_configured_agent() {
    let cfg = configured_agent();

    let error = dotfiles_launch_gate(
        &cfg,
        "claude",
        anyhow::anyhow!("cannot create HOME").context("bundle dotfiles into /tmp/bundle-home"),
    )
    .expect_err("configured agents must not launch with an unprepared HOME");

    assert!(error.to_string().contains("refusing to launch `claude`"));
    assert!(error.to_string().contains("/tmp/bundle-home"));
}

#[test]
fn dotfiles_failure_lets_a_plain_shell_launch() {
    let cfg = configured_agent();

    assert!(dotfiles_launch_gate(&cfg, "shell", anyhow::anyhow!("x")).is_ok());
}
