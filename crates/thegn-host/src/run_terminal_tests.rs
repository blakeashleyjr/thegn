use super::terminal_launch_is_current;
use std::path::Path;

#[test]
fn terminal_launch_from_prior_generation_is_rejected() {
    assert!(!terminal_launch_is_current(
        4,
        Path::new("/worktrees/old"),
        5,
        Path::new("/worktrees/new"),
    ));
}

#[test]
fn rejected_terminal_location_has_no_host_cwd_fallback() {
    let location = thegn_core::remote::GitLoc::Provider {
        control_prefix: vec!["sprite".into(), "exec".into()],
        path: "/workspace/project".into(),
    };
    let result = location.interactive_argv(&["git".into(), "status".into()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not supported"));
}
