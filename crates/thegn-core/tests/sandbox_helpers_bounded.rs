//! THE-307: the podman/docker helper calls in the sandbox integration tests run through `support/bounded_run.rs`
//! (a total deadline and a byte cap). A bare `Command::output()` there would hang the whole test binary on a stuck
//! container runtime.

const FILES: [(&str, &str); 6] = [
    (
        "sandbox_credentials.rs",
        include_str!("sandbox_credentials.rs"),
    ),
    (
        "sandbox_network_policy.rs",
        include_str!("sandbox_network_policy.rs"),
    ),
    ("sandbox_health.rs", include_str!("sandbox_health.rs")),
    ("sandbox_lifecycle.rs", include_str!("sandbox_lifecycle.rs")),
    ("sandbox_profile.rs", include_str!("sandbox_profile.rs")),
    ("devcontainer_e2e.rs", include_str!("devcontainer_e2e.rs")),
];

#[test]
fn podman_and_docker_helpers_run_through_the_bounded_runner() {
    for (name, src) in FILES {
        assert!(
            src.contains("mod bounded_run;"),
            "{name} must include support/bounded_run.rs"
        );
        for (index, line) in src.lines().enumerate() {
            assert!(
                !line.contains(".output()")
                    && !line.contains(".status()")
                    && !line.contains(".spawn()"),
                "{name}:{}: use bounded_run::output instead of an unbounded process call: {line}",
                index + 1
            );
        }
    }
}
