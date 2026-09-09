//! Bounded process runner for Git commits created without an attached user.
//!
//! Commit messages still need stdin, so merely redirecting stdin to `/dev/null`
//! is not an option. This runner closes the message pipe before waiting and
//! removes every prompt surface. On Unix, a timeout kills the isolated process
//! group; other platforms kill and reap the direct child. It is intentionally
//! not used for general Git writes: remote fetch/push operations must not
//! inherit this local-commit deadline.

use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};
use thegn_core::remote::GitLoc;

use super::GIT_SPAWNS;

const MAX_CAPTURE_BYTES: u64 = 1024 * 1024;

/// A background commit exceeded its dedicated non-interactive deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundCommitTimeout {
    pub command: String,
    pub timeout: Duration,
}

impl std::fmt::Display for BackgroundCommitTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "background git commit `{}` timed out after {:?} and was killed; a signing or hook subprocess may be waiting for input — configure non-interactive signing or disable signing for this background path",
            self.command, self.timeout
        )
    }
}

impl std::error::Error for BackgroundCommitTimeout {}

const BACKGROUND_COMMIT_TIMEOUT_ENV: &str = "THEGN_BACKGROUND_COMMIT_TIMEOUT_SECS";

fn background_commit_timeout() -> Duration {
    let seconds = match std::env::var(BACKGROUND_COMMIT_TIMEOUT_ENV) {
        Ok(value) => value.trim().parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };
    let seconds = Some(seconds).filter(|seconds| *seconds > 0).unwrap_or(120);
    Duration::from_secs(seconds)
}

/// Run an automated `commit`/`commit-tree` with its message on stdin.
///
/// The message pipe is written and then closed; no terminal, graphical
/// askpass, or inherited GPG tty is available afterwards. On timeout the whole
/// Unix process group is killed and the direct child is always reaped.
pub(crate) fn run(
    loc: &GitLoc,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: &[u8],
) -> Result<String> {
    run_with_timeout(loc, envs, args, stdin, background_commit_timeout())
}

pub(crate) fn run_with_timeout(
    loc: &GitLoc,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: &[u8],
    timeout: Duration,
) -> Result<String> {
    let _lock = (!loc.is_remote())
        .then(|| thegn_core::util::lock_git_mutations(std::path::Path::new(&loc.path())))
        .flatten();
    let mut env: Vec<(&str, &str)> = vec![
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ASKPASS", ""),
        ("SSH_ASKPASS_REQUIRE", "never"),
        ("GPG_TTY", ""),
        ("DISPLAY", ""),
        ("WAYLAND_DISPLAY", ""),
    ];
    env.extend_from_slice(envs);
    // Regular files, rather than pipe-reader threads, make output collection
    // safe when a signer/hook forks and exits while a descendant retains its
    // stdout/stderr handles. Reads snapshot a bounded tail after Git exits;
    // they never wait for every inherited writer to close.
    let mut stdout =
        tempfile::NamedTempFile::new().context("create background git stdout capture")?;
    let mut stderr =
        tempfile::NamedTempFile::new().context("create background git stderr capture")?;
    let stdout_child = stdout
        .reopen()
        .context("clone background git stdout capture")?;
    let stderr_child = stderr
        .reopen()
        .context("clone background git stderr capture")?;
    let mut cmd = loc.git_command_env(&env, args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::from(stdout_child))
        .stderr(Stdio::from(stderr_child));
    isolate_process(&mut cmd);
    GIT_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("git {}", args.join(" ")))?;
    let pid = child.id();
    let mut input = child.stdin.take().expect("piped stdin");
    let bytes = stdin.to_vec();
    let (input_tx, input_rx) = std::sync::mpsc::sync_channel(1);
    let input_h = std::thread::spawn(move || {
        let result = input.write_all(&bytes);
        drop(input); // EOF tells `-F -` that the commit message is complete.
        // A closed receiver means the deadline path already owns cleanup.
        if input_tx.send(result).is_err() {}
    });
    let deadline = Instant::now() + timeout;
    // Finish and close the sole stdin pipe before waiting for the commit. Git
    // cannot accidentally hand the commit-message stream to a signing prompt.
    let input_result = match input_rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            kill_process_tree(pid);
            kill_and_reap(&mut child);
            finish_after_kill(input_h);
            return Err(timeout_error(args, timeout));
        }
    };
    input_h
        .join()
        .map_err(|_| anyhow::anyhow!("background git stdin writer panicked"))?;
    let mut backoff = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                kill_process_tree(pid);
                kill_and_reap(&mut child);
                return Err(error).context("wait for background git commit");
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            kill_process_tree(pid);
            kill_and_reap(&mut child);
            return Err(timeout_error(args, timeout));
        }
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(Duration::from_millis(25));
    };

    // Git's outcome is final. Do not signal its former process group after
    // `try_wait` has reaped it: if no descendants remain, that numeric group id
    // is no longer owned and could theoretically be reused. Regular-file
    // captures keep inspection bounded even when a descendant does remain.
    let stdout =
        read_capture(stdout.as_file_mut()).context("read background git stdout capture")?;
    let stderr =
        read_capture(stderr.as_file_mut()).context("read background git stderr capture")?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let stdout_text = String::from_utf8_lossy(&stdout);
        let detail = match (stderr.trim(), stdout_text.trim()) {
            ("", "") => String::new(),
            ("", output) => output.to_string(),
            (error, "") => error.to_string(),
            (error, output) => format!("{error}\n{output}"),
        };
        anyhow::bail!("git {} failed: {}", args.join(" "), detail);
    }
    input_result.context("write background git commit message")?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn timeout_error(args: &[&str], timeout: Duration) -> anyhow::Error {
    BackgroundCommitTimeout {
        command: args.join(" "),
        timeout,
    }
    .into()
}

fn read_capture(file: &mut std::fs::File) -> std::io::Result<Vec<u8>> {
    let total = file.metadata()?.len();
    let len = total.min(MAX_CAPTURE_BYTES);
    file.seek(SeekFrom::Start(total.saturating_sub(len)))?;
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn kill_and_reap(child: &mut std::process::Child) {
    // Cleanup is best effort because callers must retain the primary timeout
    // or wait error. `wait` is still attempted when `kill` reports that the
    // child has already exited so no zombie is left behind.
    drop(child.kill());
    drop(child.wait());
}

#[cfg(unix)]
fn isolate_process(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // A new session removes access to the compositor's controlling terminal;
    // its process-group id is the child pid, which also gives timeout cleanup a
    // precise descendant target.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn isolate_process(_cmd: &mut std::process::Command) {}

#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    // The child is the process-group leader after `setsid` above.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_tree(_pid: u32) {}

// Unix has killed the whole isolated group, so a blocked stdin writer can be
// joined. On other platforms only Git is guaranteed terminated; a signer may
// still own the read end. Dropping this handle keeps the caller bounded.
#[cfg(unix)]
fn finish_after_kill<T>(handle: std::thread::JoinHandle<T>) {
    // The process group is gone, so joining cannot wait on a live pipe. A
    // writer panic cannot replace the primary timeout returned by the caller.
    drop(handle.join());
}

#[cfg(not(unix))]
fn finish_after_kill<T>(_handle: std::thread::JoinHandle<T>) {}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::{BackgroundCommitTimeout, run_with_timeout};
    use crate::git::testutil::{TestRepo, git_in};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn timeout_kills_signer_tree_reaps_git_and_preserves_ref() {
        let repo = TestRepo::new("plumb-sign-timeout");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        let tree = repo.out(&["rev-parse", "HEAD^{tree}"]);
        let pid_file = repo.dir.join("hung-signer.pid");
        let signer = repo.dir.join("hung-signer.sh");
        std::fs::write(
            &signer,
            format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&signer, std::fs::Permissions::from_mode(0o755)).unwrap();
        git_in(
            &repo.dir,
            &["config", "gpg.program", signer.to_str().unwrap()],
        );

        let error = run_with_timeout(
            &loc,
            &[],
            &["commit-tree", &tree, "-p", &head, "-S"],
            b"signed fold\n",
            std::time::Duration::from_millis(500),
        )
        .expect_err("the hanging signer must hit the background deadline");
        assert!(
            error.downcast_ref::<BackgroundCommitTimeout>().is_some(),
            "timeout remains actionable and typed: {error:#}"
        );
        assert!(error.to_string().contains("500ms"), "{error:#}");
        assert!(error.to_string().contains("signing or hook"), "{error:#}");
        assert_eq!(repo.head(), head, "commit-tree failure must not move HEAD");

        let signer_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("hanging signer recorded its child")
            .parse()
            .unwrap();
        let gone = (0..100).any(|_| {
            let dead = unsafe { libc::kill(signer_pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !dead {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            dead
        });
        assert!(gone, "timed-out signer descendant {signer_pid} survived");
    }

    #[test]
    fn does_not_wait_for_output_handles_inherited_by_a_descendant() {
        // A signer has a separate Git status fd, so retaining that fd correctly
        // keeps Git itself alive and exercises the ordinary deadline. A hook
        // isolates the post-Git-exit case: its child retains stdout/stderr only.
        let repo = TestRepo::new("plumb-hook-output-child");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let head = repo.head();
        let pid_file = repo.dir.join("output-holder.pid");
        let hook = repo.dir.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nexit 1\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(repo.dir.join("staged.txt"), "staged\n").unwrap();
        git_in(&repo.dir, &["add", "staged.txt"]);

        let started = std::time::Instant::now();
        let deadline = std::time::Duration::from_secs(2);
        let error = run_with_timeout(
            &loc,
            &[],
            &["commit", "-F", "-"],
            b"background commit\n",
            deadline,
        )
        .expect_err("the hook exits unsuccessfully");
        assert!(
            started.elapsed() < deadline,
            "inherited output handles held the caller open for {:?}: {error:#}",
            started.elapsed()
        );
        assert!(error.to_string().contains("failed"), "{error:#}");
        assert_eq!(repo.head(), head);

        let holder_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("hook recorded the output-holder child")
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(holder_pid, 0) },
            0,
            "fixture child should still be alive while retaining the captures"
        );
        unsafe {
            libc::kill(holder_pid, libc::SIGKILL);
        }
        let gone = (0..100).any(|_| {
            let dead = unsafe { libc::kill(holder_pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !dead {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            dead
        });
        assert!(
            gone,
            "fixture could not clean output-holder child {holder_pid}"
        );
    }
}
