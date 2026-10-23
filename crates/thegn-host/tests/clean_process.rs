#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fixture {
    _root: tempfile::TempDir,
    path: PathBuf,
}

struct RestoreMode {
    path: PathBuf,
    mode: u32,
    armed: bool,
}

impl RestoreMode {
    fn new(path: &Path, mode: u32) -> Self {
        Self {
            path: path.to_path_buf(),
            mode,
            armed: true,
        }
    }

    fn restore(&mut self) -> std::io::Result<()> {
        fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for RestoreMode {
    #[allow(
        clippy::disallowed_macros,
        reason = "best-effort fixture teardown reporting"
    )]
    fn drop(&mut self) {
        if self.armed
            && let Err(error) =
                fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode))
        {
            eprintln!("best-effort: failed to restore fixture permissions: {error}");
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_path_buf();
        for name in [
            "cwd",
            "config",
            "state",
            "cache",
            "runtime",
            "local",
            "empty-path",
        ] {
            fs::create_dir(path.join(name)).unwrap();
        }
        Self { _root: root, path }
    }

    fn command(&self, args: &[&str], active: Option<&Path>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_thegn"));
        command
            .env_clear()
            .current_dir(self.path.join("cwd"))
            .stdin(Stdio::null())
            .env("THEGN_DIR", self.path.join("app"))
            .env("XDG_CONFIG_HOME", self.path.join("config"))
            .env("XDG_STATE_HOME", self.path.join("state"))
            .env("XDG_CACHE_HOME", self.path.join("cache"))
            .env("XDG_RUNTIME_DIR", self.path.join("runtime"))
            .env("TMPDIR", &self.path)
            .env("APPDATA", self.path.join("config"))
            .env("LOCALAPPDATA", self.path.join("local"))
            .env("PATH", self.path.join("empty-path"));
        if let Some(active) = active {
            command.env("THEGN_WORKTREE", active);
        }
        command.args(args);
        command
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let worktree = self.path.join(name);
        fs::create_dir_all(worktree.join("src")).unwrap();
        fs::write(worktree.join("Cargo.toml"), b"[package]\nname='fixture'\n").unwrap();
        fs::write(worktree.join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::create_dir_all(worktree.join("target/debug")).unwrap();
        fs::write(worktree.join("target/debug/x"), b"build artifact\n").unwrap();
        worktree
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "integration test runs the real binary"
)]
fn run(fixture: &Fixture, args: &[&str], active: Option<&Path>) -> Output {
    fixture.command(args, active).output().unwrap()
}

#[test]
fn cli_clean_removes_target_and_keeps_the_checkout() {
    let fixture = Fixture::new();
    for (name, args) in [
        ("wt-clean", vec!["wt", "clean"]),
        ("legacy-clean", vec!["clean"]),
    ] {
        let worktree = fixture.worktree(name);
        let mut argv = args;
        argv.extend(["--worktree", worktree.to_str().unwrap(), "--force"]);
        let output = run(&fixture, &argv, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("cleaned "), "{stdout}");
        assert!(stdout.contains("across 1 worktree(s)."), "{stdout}");
        assert!(!worktree.join("target").exists());
        assert!(worktree.join("src/main.rs").is_file());
        assert!(worktree.join("Cargo.toml").is_file());
    }
}

#[test]
fn cli_clean_skips_the_active_worktree() {
    let fixture = Fixture::new();
    let worktree = fixture.worktree("active");
    let output = run(
        &fixture,
        &[
            "wt",
            "clean",
            "--worktree",
            worktree.to_str().unwrap(),
            "--force",
        ],
        Some(&worktree),
    );
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("skip (active worktree)")
    );
    assert!(worktree.join("target").is_dir());
}

#[test]
fn cli_clean_without_force_declines_on_eof() {
    let fixture = Fixture::new();
    let worktree = fixture.worktree("declined");
    let output = run(
        &fixture,
        &["wt", "clean", "--worktree", worktree.to_str().unwrap()],
        None,
    );
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!("skip: {}", worktree.display())),
        "{stdout}"
    );
    assert!(worktree.join("target").is_dir());
}

#[test]
#[allow(clippy::disallowed_macros, reason = "test skip notice")]
fn cli_clean_reports_a_failed_target_nonzero() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping permission failure case: root can remove mode-restricted files");
        return;
    }
    let fixture = Fixture::new();
    let worktree = fixture.worktree("locked");
    let locked = worktree.join("target/locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("payload"), b"locked\n").unwrap();
    let original_mode = fs::metadata(&locked).unwrap().permissions().mode();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    let mut restore_mode = RestoreMode::new(&locked, original_mode);

    let output = run(
        &fixture,
        &[
            "wt",
            "clean",
            "--worktree",
            worktree.to_str().unwrap(),
            "--force",
        ],
        None,
    );
    restore_mode.restore().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("failed to clean")
    );
    assert!(locked.join("payload").is_file());
}
