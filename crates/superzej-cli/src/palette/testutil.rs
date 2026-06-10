//! Test-only helpers shared across the palette's test modules.

use std::path::PathBuf;
use std::sync::OnceLock;

static SANDBOX: OnceLock<PathBuf> = OnceLock::new();

/// Redirect all state/config/cache (and the zellij socket) into a throwaway
/// per-process temp dir, so DB- and zellij-touching tests never read or write
/// the real superzej state. Idempotent; safe to call from every test.
pub fn sandbox() -> PathBuf {
    SANDBOX
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("sz-palette-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("HOME", &dir);
            std::env::set_var("XDG_STATE_HOME", dir.join("state"));
            std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
            std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
            std::env::set_var("XDG_DATA_HOME", dir.join("data"));
            std::env::set_var("ZELLIJ_SOCKET_DIR", dir.join("run"));
            std::env::remove_var("ZELLIJ");
            std::env::remove_var("ZELLIJ_SESSION_NAME");
            // Scrub inherited git env: when the suite runs inside a git hook
            // (e.g. prek's pre-commit), git exports GIT_DIR / GIT_WORK_TREE /
            // GIT_INDEX_FILE pointing at the *real* repo. Those leak into the
            // `git` children these tests spawn (temp_git_repo, branches), so a
            // temp dir is treated as the real repo. Remove them for isolation.
            for var in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_COMMON_DIR",
                "GIT_PREFIX",
                "GIT_CONFIG",
                "GIT_CONFIG_GLOBAL",
            ] {
                std::env::remove_var(var);
            }
            dir
        })
        .clone()
}

/// Create an initialized git repo under `sandbox()/name` with one commit on a
/// branch named `main`, returning its path.
pub fn temp_git_repo(name: &str) -> PathBuf {
    let root = sandbox().join(name);
    let _ = std::fs::create_dir_all(&root);
    let run = |args: &[&str]| {
        let _ = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output();
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@t.t"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-qm", "init"]);
    root
}
