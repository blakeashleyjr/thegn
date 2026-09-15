//! Owned actual-binary fixture. Private paths are configuration, not an OS sandbox.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LIMIT: u64 = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);

#[path = "../../../../test/support/owned_test_child.rs"]
mod owned_test_child;
use owned_test_child::{OwnedChild, ProcessObservation};

pub struct Fixture {
    root: Arc<tempfile::TempDir>,
    serial: usize,
}
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
}
impl Fixture {
    pub fn new() -> Self {
        let root = Arc::new(tempfile::tempdir().unwrap());
        for name in [
            "cwd",
            "config",
            "state",
            "cache",
            "runtime",
            "local",
            "empty-path",
            "outputs",
        ] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        Self { root, serial: 0 }
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    pub fn binary(&self) -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_thegn"))
    }
    pub fn legacy(&self) {
        for parent in ["config", "state", "local"] {
            let path = self.path(parent).join("superzej");
            fs::create_dir(&path).unwrap();
            fs::write(path.join("sentinel"), b"private legacy bytes\n").unwrap();
        }
    }
    pub fn command(
        &self,
        binary: &Path,
        args: &[&str],
        config: Option<&Path>,
        profile_env: bool,
    ) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .current_dir(self.path("cwd"))
            .stdin(Stdio::null())
            .env("THEGN_DIR", self.path("app"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .env("APPDATA", self.path("config"))
            .env("LOCALAPPDATA", self.path("local"))
            .env("PATH", self.path("empty-path"));
        if let Some(config) = config {
            command.arg("--config").arg(config);
        }
        if profile_env {
            command.env("THEGN_PROFILE", "fixture-profile");
        }
        command.args(args);
        command
    }
    pub fn run(&mut self, args: &[&str], config: Option<&Path>, profile_env: bool) -> Output {
        self.run_binary(&self.binary(), args, config, profile_env, None)
    }
    pub fn run_binary(
        &mut self,
        binary: &Path,
        args: &[&str],
        config: Option<&Path>,
        profile_env: bool,
        stdout: Option<Stdio>,
    ) -> Output {
        let command = self.command(binary, args, config, profile_env);
        self.execute(command, stdout, None)
            .expect("static child must finish")
    }
    pub fn assert_loader_wait(&mut self, args: &[&str], config: &Path) {
        let command = self.command(&self.binary(), args, Some(config), false);
        assert!(
            self.execute(command, None, Some(Duration::from_millis(500)))
                .is_none(),
            "configured counterpart unexpectedly passed the unread FIFO"
        );
    }
    fn execute(
        &mut self,
        mut command: Command,
        stdout: Option<Stdio>,
        expect_blocked: Option<Duration>,
    ) -> Option<Output> {
        self.serial += 1;
        let out_path = self.path("outputs").join(format!("{}-stdout", self.serial));
        let err_path = self.path("outputs").join(format!("{}-stderr", self.serial));
        let out = File::create(&out_path).unwrap();
        command
            .stdout(stdout.unwrap_or_else(|| Stdio::from(out)))
            .stderr(File::create(&err_path).unwrap());
        let mut child = OwnedChild::spawn(&mut command, Arc::clone(&self.root));
        // Command also owns Stdio handles; close these before strict root cleanup.
        drop(command);
        let until = Instant::now() + expect_blocked.unwrap_or(DEADLINE);
        loop {
            for path in [&out_path, &err_path] {
                assert!(
                    fs::metadata(path).unwrap().len() <= LIMIT,
                    "owned CLI output exceeded limit"
                );
            }
            if let Some(status) = child.poll() {
                let output = Output {
                    status,
                    stdout: bounded_text(&out_path),
                    stderr: bounded_text(&err_path),
                };
                assert!(
                    expect_blocked.is_none(),
                    "configured child exited before FIFO deadline: {:?}: {}",
                    output.status,
                    output.stderr
                );
                return Some(output);
            }
            if Instant::now() >= until {
                assert!(
                    child.terminate(DEADLINE),
                    "owned CLI child could not be reaped; retained custody"
                );
                if expect_blocked.is_some() {
                    return None;
                }
                panic!(
                    "owned CLI deadline; stdout={}, stderr={}",
                    bounded_text(&out_path),
                    bounded_text(&err_path)
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    pub fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, current: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let mut entries = fs::read_dir(current)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if relative.starts_with("outputs") {
                    continue;
                }
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    result.insert(relative, b"directory".to_vec());
                    visit(root, &path, result);
                } else if kind.is_file() {
                    result.insert(relative, fs::read(&path).unwrap());
                } else {
                    result.insert(relative, b"nonregular-owned-fixture".to_vec());
                }
            }
        }
        let mut result = BTreeMap::new();
        visit(self.root.path(), self.root.path(), &mut result);
        result
    }
    pub fn close(self) {
        Arc::try_unwrap(self.root)
            .expect("unsettled child retains private fixture")
            .close()
            .unwrap();
    }
}

pub fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}; stderr: {}",
        output.status,
        output.stderr
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected static diagnostic: {}",
        output.stderr
    );
}

fn bounded_text(path: &Path) -> String {
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(
        bytes.len() as u64 <= LIMIT,
        "owned CLI output exceeded limit"
    );
    String::from_utf8(bytes).unwrap()
}

#[test]
fn child_observation_error_revokes_later_signals_and_waits() {
    use std::cell::Cell;
    for after_prior_observation in [false, true] {
        let mut observation = ProcessObservation::default();
        let calls = Cell::new(0);
        if after_prior_observation {
            assert_eq!(
                observation
                    .observe(|| {
                        calls.set(calls.get() + 1);
                        Ok(None::<ExitStatus>)
                    })
                    .unwrap(),
                None
            );
        }
        let result: std::io::Result<Option<ExitStatus>> = observation.observe(|| {
            calls.set(calls.get() + 1);
            Err(std::io::Error::other(
                "injected child identity observation failure",
            ))
        });
        assert!(result.is_err());
        let at_loss = calls.get();
        for _ in 0..3 {
            assert!(
                observation
                    .if_owned(|| {
                        calls.set(calls.get() + 1);
                    })
                    .is_none()
            );
            assert!(
                observation
                    .observe(|| {
                        calls.set(calls.get() + 1);
                        Ok(None::<ExitStatus>)
                    })
                    .is_err()
            );
        }
        assert_eq!(
            calls.get(),
            at_loss,
            "cleanup must not signal or observe after lost identity"
        );
    }
}
