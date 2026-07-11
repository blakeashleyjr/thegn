//! Suite: devcontainer.json end-to-end (Tier 2, podman/docker required).
//!
//! Runs the REAL pipeline — parse a `.devcontainer/devcontainer.json`, fold it
//! onto a `SandboxConfig` via the overlay, resolve a `SandboxSpec`, and
//! `ensure()` a live container — then execs into it to assert the observable
//! result. Skips unless `PODMAN_E2E_FORCE` is set (same gate as
//! `sandbox_lifecycle.rs`), so it never runs in the machine-independent CI.

use std::path::{Path, PathBuf};

use thegn_core::config::{SandboxBackend, SandboxConfig, SandboxProfile};
use thegn_core::devcontainer::{self, SubstCtx};
use thegn_core::devcontainer_overlay;
use thegn_core::remote::GitLoc;
use thegn_core::sandbox::{self, container_name};

fn skip() -> bool {
    !thegn_core::util::have("podman")
        || std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
}

fn podman(args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("podman").args(args).output();
    match out {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// Exec a shell snippet in the running container, returning trimmed stdout.
fn exec_in(name: &str, script: &str) -> String {
    podman(&["exec", name, "sh", "-lc", script]).1
}

fn force_rm(name: &str) {
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", name])
        .output();
}

/// Write a temp worktree with a `.devcontainer/` holding the given files.
fn worktree_with(files: &[(&str, &str)]) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sz-dc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join(".devcontainer")).unwrap();
    for (rel, body) in files {
        let p = base.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }
    base
}

/// The overlaid config for a worktree: parse its devcontainer.json and fold it
/// onto an OCI-podman `SandboxConfig` through the real overlay (ungated apply —
/// the trust gate is tested separately in unit tests).
fn overlaid_config(worktree: &Path) -> SandboxConfig {
    let dc = devcontainer::detect_and_parse(worktree)
        .expect("detect")
        .expect("parse");
    let mut sb = SandboxConfig {
        enabled: true,
        backend: SandboxBackend::Podman,
        // Open profile: no cap drops / read-only-root surprises for the assertions.
        profile: SandboxProfile::Open,
        ..SandboxConfig::default()
    };
    let wt = worktree.to_string_lossy().into_owned();
    let ctx = SubstCtx {
        local_workspace_folder: wt.clone(),
        container_workspace_folder: wt,
        local_env: &|k| std::env::var(k).ok(),
        container_env: &|_| None,
    };
    devcontainer_overlay::apply_to_sandbox(&dc, &mut sb, &ctx);
    sb
}

fn spec_for(worktree: &Path, sb: &SandboxConfig, name: &str) -> sandbox::SandboxSpec {
    let loc = GitLoc::for_worktree(worktree);
    sandbox::resolve(sb, &loc, name).expect("resolve produced no spec")
}

// ── E1: `build.dockerfile` actually builds + runs the image ──────────────────

#[test]
fn e1_build_dockerfile_builds_and_runs() {
    if skip() {
        return;
    }
    let wt = worktree_with(&[
        (
            ".devcontainer/devcontainer.json",
            r#"{ "build": { "dockerfile": "Dockerfile", "context": "." } }"#,
        ),
        (
            ".devcontainer/Dockerfile",
            "FROM docker.io/library/alpine:latest\nRUN echo built-by-devcontainer > /dc-marker\n",
        ),
    ]);
    let name = "thegn-dc-e2e-build";
    force_rm(name);

    let sb = overlaid_config(&wt);
    // The overlay must have set a build + a local tag (not a pulled image).
    assert!(sb.build.is_some(), "overlay did not set sb.build");
    assert!(sb.image.starts_with("thegn-dc-"), "image={}", sb.image);

    let spec = spec_for(&wt, &sb, name);
    sandbox::ensure(&spec).expect("ensure (build + run) failed");

    // The container is up on the freshly BUILT image — the marker only exists
    // because the Dockerfile's RUN executed during `ensure`'s build step.
    let marker = exec_in(name, "cat /dc-marker");
    assert_eq!(
        marker, "built-by-devcontainer",
        "Dockerfile RUN did not take"
    );

    force_rm(name);
    let _ = std::fs::remove_dir_all(&wt);
}

// ── E2: image + containerEnv + postStartCommand land in the pane env ─────────

#[test]
fn e2_image_env_and_poststart() {
    if skip() {
        return;
    }
    let wt = worktree_with(&[(
        ".devcontainer/devcontainer.json",
        r#"{
            "image": "docker.io/library/alpine:latest",
            "containerEnv": { "TG_DC_ENV": "hello-from-devcontainer" },
            "postStartCommand": "echo poststart-ran > /tmp/sz-ps"
        }"#,
    )]);
    let name = "thegn-dc-e2e-env";
    force_rm(name);

    let sb = overlaid_config(&wt);
    assert_eq!(sb.image, "docker.io/library/alpine:latest");
    // env + postStart were folded into the pane init_script.
    assert!(sb.init_script.contains("export TG_DC_ENV="));
    assert!(sb.init_script.contains("poststart-ran"));

    let spec = spec_for(&wt, &sb, name);
    sandbox::ensure(&spec).expect("ensure failed");

    // Run the pane init_script the way `enter_argv`/`wrap_script` would, then
    // observe its effects: the env export is visible and postStart ran.
    let init = spec.init_script.clone().unwrap_or_default();
    let env_seen = exec_in(name, &format!("{init}\nprintf %s \"$TG_DC_ENV\""));
    assert_eq!(
        env_seen, "hello-from-devcontainer",
        "containerEnv not applied"
    );
    let ps = exec_in(name, "cat /tmp/sz-ps 2>/dev/null");
    assert_eq!(ps, "poststart-ran", "postStartCommand did not run");

    force_rm(name);
    let _ = std::fs::remove_dir_all(&wt);
}

// ── E3: dockerComposeFile brings up the service; pane enters via compose exec ─

#[test]
fn e3_compose_up_and_exec() {
    if skip() {
        return;
    }
    // docker compose v2 is required for this one specifically.
    if !thegn_core::util::have("docker") {
        return;
    }
    let wt = worktree_with(&[
        (
            ".devcontainer/devcontainer.json",
            r#"{ "dockerComposeFile": "docker-compose.yml", "service": "app" }"#,
        ),
        (
            ".devcontainer/docker-compose.yml",
            "services:\n  app:\n    image: docker.io/library/alpine:latest\n    command: sleep infinity\n",
        ),
    ]);
    let name = "thegn-dc-e2e-compose";

    let sb = overlaid_config(&wt);
    let cs = thegn_core::sandbox_compose::ComposeSpec::decode(sb.compose.as_ref().unwrap());
    assert_eq!(cs.service.as_deref(), Some("app"));
    assert_eq!(cs.files.len(), 1);

    let spec = spec_for(&wt, &sb, name);
    // Tear down any prior project first, then bring it up.
    let _ = std::process::Command::new("docker")
        .args(["compose", "-p", name, "down", "-t", "1"])
        .output();
    sandbox::ensure(&spec).expect("ensure (compose up) failed");

    // The pane-enter argv must route through `docker compose exec app`.
    let argv = sandbox::enter_argv(&spec, "true");
    let joined = argv.join(" ");
    assert!(
        joined.contains("compose") && joined.contains("exec") && joined.contains("app"),
        "enter_argv did not use compose exec: {joined}"
    );

    // Exercise the real exec into the service (non-interactive `-T`).
    let out = std::process::Command::new("docker")
        .args([
            "compose",
            "-p",
            name,
            "-f",
            &wt.join(".devcontainer/docker-compose.yml")
                .to_string_lossy(),
            "exec",
            "-T",
            "app",
            "sh",
            "-c",
            "echo compose-ok",
        ])
        .output()
        .expect("compose exec");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "compose-ok");

    let _ = std::process::Command::new("docker")
        .args(["compose", "-p", name, "down", "-t", "1"])
        .output();
    let _ = std::fs::remove_dir_all(&wt);
    let _ = container_name; // silence unused in the docker-only branch
}
