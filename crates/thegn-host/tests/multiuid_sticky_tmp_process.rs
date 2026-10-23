use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(180);
const CAPTURE_LIMIT: u64 = 64 * 1024;

#[expect(
    clippy::disallowed_methods,
    reason = "process test owns and reaps the namespace driver"
)]
fn run_script(script: &Path, bin: &Path, out: &Path, err: &Path, timeout: Duration) -> ExitStatus {
    let stdout = File::create(out).expect("create namespace-driver stdout capture");
    let stderr = File::create(err).expect("create namespace-driver stderr capture");
    let mut command = Command::new("bash");
    let mut child: Child = command
        .arg(script)
        .arg(bin)
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").expect("test runner PATH is set"),
        )
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn sticky-tmp namespace driver");

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                child.kill().expect("kill timed-out namespace driver");
                let _status = child.wait().expect("reap timed-out namespace driver");
                panic!(
                    "sticky-tmp namespace driver exceeded {timeout:?}; stdout: {}; stderr: {}",
                    read_capture(out),
                    read_capture(err)
                );
            }
            Err(error) => {
                child
                    .kill()
                    .expect("kill namespace driver after poll failure");
                let _status = child
                    .wait()
                    .expect("reap namespace driver after poll failure");
                panic!("could not poll sticky-tmp namespace driver: {error}");
            }
        }
    }
}

fn read_capture(path: &Path) -> String {
    let file = File::open(path).expect("open namespace-driver capture");
    let mut bytes = Vec::new();
    file.take(CAPTURE_LIMIT)
        .read_to_end(&mut bytes)
        .expect("read namespace-driver capture");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn multiuid_sticky_tmp_foreign_predictable_entries_never_block_the_daemon() {
    if !thegn_core::testenv::require_or_skip(
        "Linux (multi-uid sticky /tmp proof)",
        cfg!(target_os = "linux"),
    ) {
        return;
    }

    let captures = tempfile::tempdir().expect("create namespace-driver capture directory");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/multiuid_support/sticky_tmp.sh");
    let binary = Path::new(env!("CARGO_BIN_EXE_thegn"));
    let stdout = captures.path().join("stdout");
    let stderr = captures.path().join("stderr");
    let status = run_script(&script, binary, &stdout, &stderr, SCRIPT_TIMEOUT);
    let out = read_capture(&stdout);
    let err = read_capture(&stderr);

    if status.success() {
        assert!(
            out.contains("PASS:"),
            "namespace proof emitted no PASS lines: {out}\n{err}"
        );
        return;
    }
    if status.code() == Some(77) {
        let skip_line = out
            .lines()
            .find(|line| line.starts_with("SKIP:"))
            .unwrap_or("SKIP: namespace prerequisites unavailable");
        let reason = skip_line.trim_start_matches("SKIP:").trim();
        thegn_core::testenv::require_or_skip(
            &format!("user namespaces with subuid mappings: {reason}"),
            false,
        );
        return;
    }

    panic!("sticky-tmp namespace proof failed ({status}); stdout: {out}\nstderr: {err}");
}
