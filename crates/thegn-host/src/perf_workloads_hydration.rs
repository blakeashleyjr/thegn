//! Full model builds against real, private Git worktrees and a fixture DB.
//! Run this ignored test alone in a fresh process with a cleared environment.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::Instant;
use thegn_core::store::WorkspaceStore;

fn git(executable: &Path, cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new(executable)
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture git failed: {:?}",
        output.stderr
    );
}

#[test]
#[ignore = "controlled full hydration workload; run alone with a cleared environment"]
#[allow(
    clippy::disallowed_macros,
    reason = "opt-in fixture emits measured artifact"
)]
fn controlled_full_hydration_workload() {
    let count: usize = std::env::var("THEGN_AUDIT_WORKTREES")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    assert!([1, 8, 32].contains(&count));
    let git_path = thegn_core::util::which_path("git").unwrap();
    let shell_path = thegn_core::util::which_path("sh").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let config = root.join("config");
    let state = root.join("state");
    let runtime = root.join("runtime");
    let bin = root.join("bin");
    std::fs::create_dir_all(config.join("thegn")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    symlink(&git_path, bin.join("git")).unwrap();
    symlink(&shell_path, bin.join("sh")).unwrap();
    let _env = crate::testenv::EnvVarGuard::set(&[
        ("XDG_CONFIG_HOME", config.to_str().unwrap()),
        ("XDG_STATE_HOME", state.to_str().unwrap()),
        ("THEGN_DIR", runtime.to_str().unwrap()),
        ("PATH", bin.to_str().unwrap()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("THEGN_E2E", "0"),
        (
            "THEGN_AUDIT_PROBE_LOG",
            root.join("probes").to_str().unwrap(),
        ),
    ]);
    let mut cfg = thegn_core::config::Config::default();
    cfg.env.clear();
    cfg.placement.enabled = false;
    cfg.lifecycle.enabled = false;
    std::fs::write(
        config.join("thegn/config.toml"),
        toml::to_string(&cfg).unwrap(),
    )
    .unwrap();
    let cfg = crate::hydrate::load_hydration_config();
    assert!(!cfg.placement.enabled && !cfg.lifecycle.enabled);
    assert!(!cfg.env.values().any(|e| e.provider.hibernate_enabled()
        || thegn_core::config::vps_provider_kind(&e.provider.provider)
        || e.provider.provider == "fly"));
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(Path::new(&git_path), &repo, &["init", "-b", "main"]);
    for n in 0..64 {
        std::fs::write(repo.join(format!("file-{n}.txt")), "fixture baseline\n").unwrap();
    }
    git(Path::new(&git_path), &repo, &["add", "."]);
    git(
        Path::new(&git_path),
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "fixture",
        ],
    );
    let db = thegn_core::db::Db::open().unwrap();
    db.put_workspace(repo.to_str().unwrap(), "fixture", "local")
        .unwrap();
    let mut session = crate::session::Session::default();
    for n in 0..count {
        let wt = if n == 0 {
            repo.clone()
        } else {
            let wt = root.join(format!("worktree-{n}"));
            git(
                Path::new(&git_path),
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &format!("fixture-{n}"),
                    wt.to_str().unwrap(),
                ],
            );
            wt
        };
        std::fs::write(wt.join("file-0.txt"), "fixture changed\n").unwrap();
        let name = format!("fixture/{n}");
        db.put_worktree(
            &name,
            repo.to_str().unwrap(),
            wt.to_str().unwrap(),
            if n == 0 { "main" } else { "fixture" },
            None,
            None,
        )
        .unwrap();
        session.worktrees.push(crate::session::WorktreeGroup::new(
            name,
            crate::session::GroupKind::Branch,
            wt.to_string_lossy(),
        ));
    }
    crate::perf::set_enabled(true);
    let mut results = Vec::new();
    for helper in [false, true] {
        if helper {
            let path = bin.join("devcontainer");
            std::fs::write(&path, format!("#!{shell_path}\nprintf 'probe\\n' >> \"$THEGN_AUDIT_PROBE_LOG\"\nprintf '0.0.0-fixture\\n'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut samples = Vec::new();
        for sample in 0..9 {
            let _ = crate::perf::CPU.take();
            let start = Instant::now();
            let model =
                crate::hydrate::build_model(&session, &db, crate::hydrate::HydrateHints::default());
            let elapsed = start.elapsed().as_micros();
            assert_eq!(model.panel.changes.len(), 1);
            assert_eq!(model.sidebar_db_worktrees.len(), count);
            let cpu = crate::perf::CPU.take();
            samples.push(serde_json::json!({"sample": sample, "wall_us": elapsed,
                "parent_cpu_ns": cpu[crate::perf::Subsys::Hydrate as usize].0,
                "child_cpu_ns": cpu[crate::perf::Subsys::HydrateChild as usize].0}));
        }
        let probes = std::fs::read_to_string(root.join("probes"))
            .unwrap_or_default()
            .lines()
            .count();
        results.push(serde_json::json!({"worktrees": count, "fixture_helper_present": helper,
            "helper_invocations_total": probes, "samples": samples,
            "scope": "actual build_model, 64 tracked files per worktree, one dirty file, no providers, no terminal; helper is an immediate local fixture; first sample may be cold"}));
    }
    crate::perf::set_enabled(false);
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}
