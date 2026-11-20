//! Seed a plain-ssh `data = "in_env"` worktree onto its host (THE-2049).
//!
//! A provider env gets its repository from the `bundle_seed` provisioning step;
//! an `[env.<name>] placement = "ssh"` env with `data = "in_env"` had nothing
//! equivalent, so the pane `cd`-ed to the LOCAL worktree path on the remote
//! host. [`seed_if_needed`] bundles the local branch, seeds it into
//! `$HOME/thegn-worktrees/<repo>/<worktree>` on the host through the shared
//! [`seed_from_bundle_with`], and
//! only then records the worktree's location as that remote checkout.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use thegn_core::config::{Config, DataMode, PlacementMode};
use thegn_core::db::Db;
use thegn_core::placement::Placement;
use thegn_core::remote::{GitLoc, SshTarget};
use thegn_core::store::WorkspaceStore;
use thegn_core::util::sh_quote;
use thegn_core::{msg, repo};

use crate::provider_git::{SeedTransport, seed_from_bundle_with};

/// Bytes of each output stream kept from a remote seed command (enough for the
/// `TG-SEED-*` markers and an error gist).
const OUTPUT_TAIL: usize = 64 * 1024;

/// [`SeedTransport`] over a plain ssh host. Every command runs under
/// `/bin/sh -c` so the remote login shell's dialect never matters.
pub(crate) struct SshSeed {
    /// `GitLoc::Remote { ssh, path: "/" }`: commands run from `/`.
    pub(crate) loc: GitLoc,
    /// The output of the last [`SeedTransport::sh`] run, so the caller can
    /// require the seed script's `TG-SEED-OK` marker before recording the row.
    last_output: RefCell<String>,
}

impl SshSeed {
    pub(crate) fn new(ssh: SshTarget) -> Self {
        SshSeed {
            loc: GitLoc::Remote {
                ssh,
                path: "/".to_string(),
            },
            last_output: RefCell::new(String::new()),
        }
    }

    fn host(&self) -> &str {
        match &self.loc {
            GitLoc::Remote { ssh, .. } => &ssh.host,
            _ => "the host",
        }
    }

    /// Run `script` remotely with `input` on stdin, bounded by the
    /// `bundle_seed` step timeout. Returns the run and whether stdin was
    /// fully delivered.
    fn run(&self, script: &str, input: &[u8]) -> anyhow::Result<(i32, String, bool)> {
        let mut cmd = self
            .loc
            .sh_command(&format!("/bin/sh -c {}", sh_quote(script)));
        let timeout = crate::agent::provision_step_timeout("bundle_seed");
        let limits = thegn_proc::Limits {
            stdout: thegn_proc::Stream::Tail { cap: OUTPUT_TAIL },
            stderr: thegn_proc::Stream::Tail { cap: OUTPUT_TAIL },
            reader_grace: std::time::Duration::from_secs(2),
            ..thegn_proc::Limits::silent(timeout)
        };
        let (run, feed) =
            thegn_proc::run_with_input(&mut cmd, &limits, input, &AtomicBool::new(false));
        let out = format!(
            "{}{}",
            String::from_utf8_lossy(&run.stdout.bytes),
            String::from_utf8_lossy(&run.stderr.bytes)
        );
        match run.end {
            thegn_proc::End::LeaderExited => {}
            thegn_proc::End::TimedOut => {
                anyhow::bail!(
                    "ssh to {} timed out after {}s",
                    self.host(),
                    timeout.as_secs()
                )
            }
            _ => {
                let why = run
                    .error()
                    .map_or_else(|| "did not finish".to_string(), |e| e.to_string());
                anyhow::bail!("ssh to {}: {why}", self.host());
            }
        }
        // A signal death has no code; report it as a failure, never success.
        let code = run.exit_status().and_then(|s| s.code()).unwrap_or(-1);
        Ok((code, out, feed.complete))
    }
}

impl SeedTransport for SshSeed {
    fn sh(&self, script: &str) -> anyhow::Result<(i32, String)> {
        let (code, out, _) = self.run(script, &[])?;
        self.last_output.replace(out.clone());
        Ok((code, out))
    }

    fn write(&self, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let dir = Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .filter(|d| !d.is_empty())
            .with_context(|| format!("{path} has no parent directory"))?;
        let script = format!(
            "umask 077; mkdir -p -- {} && cat > {}",
            sh_quote(dir),
            sh_quote(path)
        );
        let (code, out, complete) = self.run(&script, bytes)?;
        anyhow::ensure!(
            code == 0 && complete,
            "writing {path} on {} failed (exit {code}{}): {}",
            self.host(),
            if complete { "" } else { ", input cut short" },
            crate::provision_recover::stderr_gist(&out)
        );
        Ok(())
    }

    fn label(&self) -> String {
        format!("ssh host {}", self.host())
    }
}

/// Whether a seed script's output carries its success marker.
fn seed_ok(out: &str) -> bool {
    out.lines()
        .map(str::trim)
        .any(|line| line.starts_with("TG-SEED-OK"))
}

/// Whether a worktree needs seeding onto its ssh host: an ssh placement whose
/// data lives in the env, and a worktree row that is still local (absent,
/// empty or `local`). A row that already records a remote (or provider)
/// location is never re-seeded or rewritten.
pub(crate) fn seed_needed(placement: &Placement, data: DataMode, row: Option<&str>) -> bool {
    let row_is_local = row
        .map(str::trim)
        .is_none_or(|r| r.is_empty() || r == "local");
    matches!(placement, Placement::Ssh(_)) && data == DataMode::InEnv && row_is_local
}

/// The same rule as `provider_workdir::is_valid_home`: absolute, and no
/// whitespace (an ssh diagnostic leaking into the probe would otherwise be
/// accepted as part of the path).
fn is_valid_home(home: &str) -> bool {
    home.starts_with('/') && !home.chars().any(char::is_whitespace)
}

/// `name` reduced to `[A-Za-z0-9._-]` (anything else becomes `-`); `None` when
/// nothing usable is left (empty, `.` or `..`).
fn sanitize_component(name: &str) -> Option<String> {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    (!clean.is_empty() && clean.chars().any(|c| c != '.')).then_some(clean)
}

fn basename(path: &Path) -> anyhow::Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(sanitize_component)
        .with_context(|| format!("{} has no usable directory name", path.display()))
}

/// The remote checkout for `worktree`:
/// `{home}/thegn-worktrees/{repo basename}/{worktree basename}`, with both
/// basenames sanitized to `[A-Za-z0-9._-]`. Refuses a `home` that is not an
/// absolute, whitespace-free path.
pub(crate) fn remote_workdir(
    home: &str,
    repo_root: &Path,
    worktree: &Path,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        is_valid_home(home),
        "remote $HOME {home:?} is not an absolute path without whitespace"
    );
    let home = home.trim_end_matches('/');
    Ok(format!(
        "{home}/thegn-worktrees/{}/{}",
        basename(repo_root)?,
        basename(worktree)?
    ))
}

fn ssh_target(ssh: &thegn_core::placement::SshPlacement) -> SshTarget {
    SshTarget {
        host: ssh.host.clone(),
        port: ssh.port,
        forward_agent: ssh.forward_agent,
        ssh_config: ssh.ssh_config.clone(),
        jump_host: ssh.jump_host.clone(),
        identity: ssh.identity.clone(),
        extra_args: ssh.extra_args.clone(),
    }
}

/// Seed `worktree` onto its ssh host when its env is a named
/// `[env.<name>] placement = "ssh"` with `data = "in_env"` and the worktree is
/// still recorded as local; then record the remote checkout as the worktree's
/// location. `Ok(false)` when nothing applies. Blocking (runs ssh): call it off
/// the UI loop. A legacy `[sandbox.remote]` default env is never seeded.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
pub(crate) fn seed_if_needed(cfg: &Config, worktree: &str) -> anyhow::Result<bool> {
    // Cheap gate: no ssh env configured, nothing to seed (and no DB open).
    if !cfg
        .env
        .values()
        .any(|env| env.placement == PlacementMode::Ssh)
    {
        return Ok(false);
    }
    let db = Db::open().context("open the state DB")?;
    let row = db
        .location_for(worktree)
        .with_context(|| format!("read the recorded location of {worktree}"))?;
    let repo_root: PathBuf = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let root = repo_root.to_string_lossy();
    let selected = db
        .effective_env_checked(worktree, &root)
        .context("the environment selection could not be read")?;
    let mut selection = db.effective_env_selection(worktree, &root)?;
    if selected.is_some() {
        selection.name = selected;
    }
    let loc = GitLoc::from_db(worktree, row.as_deref());
    let environment = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection);
    let named_ssh = cfg
        .env
        .get(&environment.name)
        .is_some_and(|env| env.placement == PlacementMode::Ssh);
    if environment.unresolved_selection
        || !named_ssh
        || !seed_needed(&environment.placement, environment.data, row.as_deref())
    {
        return Ok(false);
    }
    let Placement::Ssh(ssh) = &environment.placement else {
        return Ok(false);
    };
    let target = ssh_target(ssh);
    let home = thegn_core::remote::remote_home(&target)
        .with_context(|| format!("cannot read $HOME on {}", target.host))?;
    let workdir = remote_workdir(&home, &repo_root, Path::new(worktree))?;
    let branch = thegn_core::util::git_out_result(
        Path::new(worktree),
        &["rev-parse", "--abbrev-ref", "HEAD"],
    )
    .map_err(|e| anyhow::anyhow!("read the branch of {worktree}: {e}"))?;
    anyhow::ensure!(
        !branch.is_empty() && branch != "HEAD",
        "{worktree} is on a detached HEAD; check out a branch to seed it onto {}",
        target.host
    );
    let transport = SshSeed::new(target.clone());
    seed_from_bundle_with(&transport, worktree, &workdir, &branch)
        .with_context(|| format!("seed {worktree} onto {}:{workdir}", target.host))?;
    anyhow::ensure!(
        seed_ok(&transport.last_output.borrow()),
        "seeding {worktree} onto {}:{workdir} exited cleanly without TG-SEED-OK; the location was not recorded",
        target.host
    );
    // NOT best-effort: without the row, sessions `cd` to the local path on the host.
    // The UPDATE is a silent no-op for an unregistered worktree, so read it back.
    let location = GitLoc::remote_db_string_for(&target, &workdir);
    let recorded = db
        .set_worktree_location(worktree, &location)
        .and_then(|()| db.location_for(worktree))
        .with_context(|| {
            format!(
                "record {}:{workdir} as the location of {worktree}",
                target.host
            )
        })?;
    anyhow::ensure!(
        recorded.as_deref() == Some(location.as_str()),
        "{worktree} is not a registered worktree; seeded {}:{workdir} but could not record it",
        target.host
    );
    msg::info(&format!(
        "seeded {} into {}:{workdir}",
        repo_root
            .file_name()
            .map_or_else(|| root.to_string(), |n| n.to_string_lossy().into_owned()),
        target.host
    ));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{remote_workdir, sanitize_component, seed_if_needed, seed_needed, seed_ok};
    use crate::provider_git::{SeedTransport, seed_from_bundle_with};
    use std::cell::RefCell;
    use std::path::Path;
    use thegn_core::config::{Config, DataMode};
    use thegn_core::placement::{Placement, ProviderPlacement, SshPlacement, TransportKind};

    fn ssh() -> Placement {
        Placement::Ssh(SshPlacement::plain(
            "box.example".into(),
            22,
            false,
            TransportKind::Ssh,
        ))
    }

    const SSH_ROW: &str = r#"{"host":"box.example","port":22,"forward_agent":false,"path":"/home/u/thegn-worktrees/r/w"}"#;

    #[test]
    fn seed_needed_only_for_a_local_ssh_in_env_row() {
        assert!(seed_needed(&ssh(), DataMode::InEnv, None));
        assert!(seed_needed(&ssh(), DataMode::InEnv, Some("")));
        assert!(seed_needed(&ssh(), DataMode::InEnv, Some("local")));
        assert!(!seed_needed(&ssh(), DataMode::InEnv, Some(SSH_ROW)));
        assert!(!seed_needed(&ssh(), DataMode::Sshfs, None));
        assert!(!seed_needed(&ssh(), DataMode::Sync, None));
        assert!(!seed_needed(&ssh(), DataMode::LocalExec, None));
        let provider = Placement::Provider(ProviderPlacement {
            provider: "fake".into(),
            id: "fake-sandbox".into(),
            interactive_prefix: Vec::new(),
            control_prefix: Vec::new(),
            up_command: Vec::new(),
            down_command: Vec::new(),
        });
        assert!(!seed_needed(&provider, DataMode::InEnv, None));
        assert!(!seed_needed(&Placement::Local, DataMode::InEnv, None));
    }

    #[test]
    fn seed_ok_requires_the_marker_line() {
        assert!(seed_ok("noise\nTG-SEED-OK seeded\n"));
        assert!(seed_ok("  TG-SEED-OK present"));
        assert!(!seed_ok(""));
        assert!(!seed_ok("fail TG-SEED-ERROR:clone"));
    }

    #[test]
    fn remote_workdir_sanitizes_and_rejects_a_bad_home() {
        assert_eq!(
            remote_workdir(
                "/home/u",
                Path::new("/src/thegn"),
                Path::new("/wt/tg-b144-x")
            )
            .unwrap(),
            "/home/u/thegn-worktrees/thegn/tg-b144-x"
        );
        assert_eq!(
            remote_workdir(
                "/home/u/",
                Path::new("/src/my repo$(x)"),
                Path::new("/wt/a;b'c")
            )
            .unwrap(),
            "/home/u/thegn-worktrees/my-repo--x-/a-b-c"
        );
        assert!(remote_workdir("home/u", Path::new("/r"), Path::new("/w")).is_err());
        assert!(remote_workdir("", Path::new("/r"), Path::new("/w")).is_err());
        assert!(
            remote_workdir(
                "/home/u disabling multiplexing",
                Path::new("/r"),
                Path::new("/w")
            )
            .is_err()
        );
        assert!(remote_workdir("/home/u", Path::new("/"), Path::new("/w")).is_err());
        assert_eq!(sanitize_component(".."), None);
        assert_eq!(sanitize_component("."), None);
        assert_eq!(sanitize_component(""), None);
        assert_eq!(sanitize_component("a b").as_deref(), Some("a-b"));
    }

    #[test]
    fn seed_if_needed_is_a_no_op_without_an_ssh_env() {
        let _state = crate::testenv::IsolatedState::new();
        assert!(!seed_if_needed(&Config::default(), "/nowhere/wt").unwrap());
    }

    #[derive(Debug, PartialEq)]
    enum Call {
        Sh(String),
        Write(String, usize),
    }

    struct Fake {
        calls: RefCell<Vec<Call>>,
        seed_reply: (i32, String),
    }

    impl SeedTransport for Fake {
        fn sh(&self, script: &str) -> anyhow::Result<(i32, String)> {
            self.calls.borrow_mut().push(Call::Sh(script.to_string()));
            if script.contains("TG-SEED-ERROR") {
                return Ok(self.seed_reply.clone());
            }
            Ok((0, String::new()))
        }

        fn write(&self, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
            assert!(bytes.starts_with(b"# v"), "a git bundle");
            self.calls
                .borrow_mut()
                .push(Call::Write(path.to_string(), bytes.len()));
            Ok(())
        }

        fn label(&self) -> String {
            "fake host".into()
        }
    }

    fn fixture(root: &Path) -> std::path::PathBuf {
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        for args in [
            &["init", "-q", "-b", "tg/x"][..],
            &["config", "user.name", "Seed Test"],
            &["config", "user.email", "seed@example.invalid"],
        ] {
            assert!(git(&wt, args));
        }
        std::fs::write(wt.join("a.txt"), "one\n").unwrap();
        assert!(git(&wt, &["add", "a.txt"]));
        assert!(git(&wt, &["commit", "-q", "-m", "first"]));
        wt
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture setup: a local git repo, bounded by the test"
    )]
    fn git(dir: &Path, args: &[&str]) -> bool {
        thegn_core::util::git_cmd(dir)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn seed_from_bundle_with_stages_writes_then_seeds() {
        let root = tempfile::tempdir().unwrap();
        let wt = fixture(root.path());
        let fake = Fake {
            calls: RefCell::new(Vec::new()),
            seed_reply: (0, "TG-SEED-OK seeded\n".into()),
        };
        seed_from_bundle_with(
            &fake,
            wt.to_str().unwrap(),
            "/home/u/thegn-worktrees/r/w",
            "tg/x",
        )
        .unwrap();
        let calls = fake.calls.into_inner();
        assert_eq!(calls.len(), 3, "{calls:?}");
        let Call::Sh(stage) = &calls[0] else {
            panic!("stage first: {calls:?}")
        };
        assert!(stage.starts_with("umask 077; mkdir -- "), "{stage}");
        let Call::Write(path, len) = &calls[1] else {
            panic!("write second: {calls:?}")
        };
        assert!(path.ends_with("/seed.bundle"), "{path}");
        assert!(*len > 0);
        let Call::Sh(seed) = &calls[2] else {
            panic!("seed third: {calls:?}")
        };
        assert!(seed.contains("TG-SEED-OK seeded"), "{seed}");
        assert!(
            seed.contains("git -C /home/u/thegn-worktrees/r/w "),
            "{seed}"
        );
        assert!(
            seed.contains(path.as_str()),
            "the seed reads the staged bundle"
        );
    }

    #[test]
    fn seed_from_bundle_with_fails_on_a_seed_error_marker() {
        let root = tempfile::tempdir().unwrap();
        let wt = fixture(root.path());
        let fake = Fake {
            calls: RefCell::new(Vec::new()),
            seed_reply: (70, "fatal: bad bundle\nTG-SEED-ERROR:clone\n".into()),
        };
        let err = seed_from_bundle_with(&fake, wt.to_str().unwrap(), "/srv/w", "tg/x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("TG-SEED-ERROR:clone"), "{msg}");
        assert!(msg.contains("exit 70"), "{msg}");
    }
}
