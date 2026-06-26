//! Remote worktree support: the ssh transport prefix and `GitLoc` — the shim
//! that lets every host-side git/gh read run either locally or on a remote box
//! over ssh.
//!
//! A worktree's "location" is persisted in the DB (`worktrees.location`): empty/
//! `local` for an ordinary on-host worktree, or a small JSON blob describing the
//! ssh target + remote path. The sidebar/panel/diff/PR code resolves a
//! `GitLoc::for_worktree(path)` and runs git/gh through it, so a remote worktree's
//! state shows up in the panel exactly like a local one — just over ssh.
//!
//! The interactive pane itself uses mosh (see `sandbox`); this module is the
//! *control plane* (always ssh, which — unlike mosh — can pipe non-interactive
//! commands and multiplex via ControlMaster).

use crate::util;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub forward_agent: bool,
}

/// The ssh argv prefix (without the host) for `port`, with ControlMaster
/// multiplexing so the panel's frequent git polls reuse one connection.
///
/// `batch` is for the *control plane* (git/gh reads, container lifecycle): it adds
/// `BatchMode=yes` so a missing key fails fast instead of prompting for a password
/// on a captured (non-interactive) channel and stealing the pane's stdin. The
/// interactive pane (mosh/ssh) passes `batch = false` so auth prompts still work.
pub fn ssh_base(port: u16, forward_agent: bool, batch: bool) -> Vec<String> {
    let mut v = vec!["ssh".to_string()];
    if port != 22 {
        v.push("-p".into());
        v.push(port.to_string());
    }
    if forward_agent {
        v.push("-A".into());
    }
    if batch {
        v.push("-o".into());
        v.push("BatchMode=yes".into());
    }
    v.push("-o".into());
    v.push("ConnectTimeout=10".into());
    // Multiplex so the panel's frequent git polls reuse one connection (and the
    // interactive pane's master serves later control-plane calls without re-auth).
    let ctl = util::superzej_dir().join("run/ssh-%r@%h:%p");
    v.push("-o".into());
    v.push("ControlMaster=auto".into());
    v.push("-o".into());
    v.push(format!("ControlPath={}", ctl.display()));
    v.push("-o".into());
    v.push("ControlPersist=300".into());
    v
}

/// Resolve the remote `$HOME` over ssh, so we can store absolute remote paths
/// (a `~` would not survive the shell-quoting in the git shim).
pub fn remote_home(ssh: &SshTarget) -> Option<String> {
    let mut argv = ssh_base(ssh.port, ssh.forward_agent, true);
    argv.push(ssh.host.clone());
    argv.push("printf %s \"$HOME\"".into());
    let out = Command::new(&argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Serialized form stored in `worktrees.location`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteLoc {
    host: String,
    port: u16,
    #[serde(default)]
    forward_agent: bool,
    path: String,
}

/// Where a worktree's git data lives — local on disk, or on a remote over ssh.
#[derive(Debug, Clone)]
pub enum GitLoc {
    Local(PathBuf),
    Remote { ssh: SshTarget, path: String },
}

impl GitLoc {
    /// Resolve a worktree path's location from the DB. Local on any miss, so
    /// existing/local worktrees behave exactly as before.
    pub fn for_worktree(path: &Path) -> GitLoc {
        let p = path.to_string_lossy().into_owned();
        let loc = crate::db::Db::open()
            .ok()
            .and_then(|db| db.location_for(&p).ok().flatten());
        Self::from_db(&p, loc.as_deref())
    }

    /// Build from a worktree path + its DB `location` column value.
    pub fn from_db(path: &str, location: Option<&str>) -> GitLoc {
        match location
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "local")
        {
            Some(json) => match serde_json::from_str::<RemoteLoc>(json) {
                Ok(r) => GitLoc::Remote {
                    ssh: SshTarget {
                        host: r.host,
                        port: r.port,
                        forward_agent: r.forward_agent,
                    },
                    path: r.path,
                },
                Err(_) => GitLoc::Local(PathBuf::from(path)),
            },
            None => GitLoc::Local(PathBuf::from(path)),
        }
    }

    /// The DB `location` string for a remote worktree (`None` => store local).
    pub fn remote_db_string(host: &str, port: u16, forward_agent: bool, path: &str) -> String {
        serde_json::to_string(&RemoteLoc {
            host: host.into(),
            port,
            forward_agent,
            path: path.into(),
        })
        .unwrap_or_default()
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, GitLoc::Remote { .. })
    }

    pub fn ssh(&self) -> Option<&SshTarget> {
        match self {
            GitLoc::Local(_) => None,
            GitLoc::Remote { ssh, .. } => Some(ssh),
        }
    }

    /// The worktree path (local or remote absolute path).
    pub fn path(&self) -> String {
        match self {
            GitLoc::Local(p) => p.to_string_lossy().into_owned(),
            GitLoc::Remote { path, .. } => path.clone(),
        }
    }

    /// A `Command` running `git -C <path> <args>` — locally, or over ssh.
    pub fn git_command(&self, args: &[&str]) -> Command {
        match self {
            GitLoc::Local(p) => {
                // Via `util::git_cmd` so the parent's repo-targeting env
                // (GIT_DIR/GIT_WORK_TREE/…) is scrubbed: this is the production
                // write layer (commits, worktree-adds, rebases), and a leaked
                // GIT_DIR pointing at the shared `.git` would make a `-C
                // <worktree>` reinit/config op write a stray `core.worktree`
                // into the shared config. See [`util::GIT_ENV_VARS`].
                let mut c = util::git_cmd(p);
                c.args(args);
                c
            }
            GitLoc::Remote { ssh, path } => self.ssh_command(ssh, {
                let mut git = vec!["git".to_string(), "-C".into(), path.clone()];
                git.extend(args.iter().map(|s| s.to_string()));
                util::sh_join(&git)
            }),
        }
    }

    /// A `Command` running `gh <args>` with cwd = the worktree — locally, or over
    /// ssh (so `gh` auto-detects the repo from its remote on the remote host).
    pub fn gh_command(&self, args: &[&str]) -> Command {
        match self {
            GitLoc::Local(p) => {
                let mut c = Command::new("gh");
                c.current_dir(p).args(args);
                c
            }
            GitLoc::Remote { ssh, path } => {
                let mut gh = vec!["gh".to_string()];
                gh.extend(args.iter().map(|s| s.to_string()));
                let remote = format!("cd {} && {}", util::sh_quote(path), util::sh_join(&gh));
                self.ssh_command(ssh, remote)
            }
        }
    }

    /// A `Command` running `<bin> <args>` with cwd = the worktree — locally, or
    /// over ssh. The generic sibling of [`Self::gh_command`], for the other CI
    /// provider CLIs (`glab`, `drone`, `woodpecker-cli`, `argo`, …). Like `gh`,
    /// these auto-detect the repo/remote from the working directory.
    pub fn cli_command(&self, bin: &str, args: &[&str]) -> Command {
        match self {
            GitLoc::Local(p) => {
                let mut c = Command::new(bin);
                c.current_dir(p).args(args);
                c
            }
            GitLoc::Remote { ssh, path } => {
                let mut argv = vec![bin.to_string()];
                argv.extend(args.iter().map(|s| s.to_string()));
                let remote = format!("cd {} && {}", util::sh_quote(path), util::sh_join(&argv));
                self.ssh_command(ssh, remote)
            }
        }
    }

    /// A `Command` running an arbitrary shell script with cwd = the worktree
    /// (the custom-command seam) — `sh -c` locally, `cd … && …` over ssh.
    pub fn sh_command(&self, script: &str) -> Command {
        match self {
            GitLoc::Local(p) => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(script).current_dir(p);
                // Custom `[[git_commands]]` scripts run arbitrary git; strip the
                // repo-targeting env so a stray GIT_DIR can't retarget them at
                // the shared `.git` (see [`util::GIT_ENV_VARS`]).
                for var in util::GIT_ENV_VARS {
                    c.env_remove(var);
                }
                c
            }
            GitLoc::Remote { ssh, path } => {
                self.ssh_command(ssh, format!("cd {} && {script}", util::sh_quote(path)))
            }
        }
    }

    fn ssh_command(&self, ssh: &SshTarget, remote_cmd: String) -> Command {
        let mut argv = ssh_base(ssh.port, ssh.forward_agent, true);
        argv.push(ssh.host.clone());
        argv.push(remote_cmd);
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    }

    /// Like [`git_command`](Self::git_command), with extra environment
    /// variables. Locally they go on the `Command`; remotely they become an
    /// `env K=V … git …` prefix inside the ssh shell string (values
    /// sh-quoted), since ssh does not forward arbitrary client env.
    pub fn git_command_env(&self, envs: &[(&str, &str)], args: &[&str]) -> Command {
        match self {
            GitLoc::Local(p) => {
                // Scrub the parent's repo-targeting env first (see
                // [`git_command`]); the caller's explicit `envs` are applied
                // after, so an intentional GIT_* override still takes effect.
                let mut c = util::git_cmd(p);
                c.args(args);
                for (k, v) in envs {
                    c.env(k, v);
                }
                c
            }
            GitLoc::Remote { ssh, path } => {
                let mut parts = vec!["env".to_string()];
                for (k, v) in envs {
                    parts.push(format!("{k}={}", util::sh_quote(v)));
                }
                parts.push("git".into());
                parts.push("-C".into());
                parts.push(util::sh_quote(path));
                parts.extend(args.iter().map(|s| util::sh_quote(s)));
                self.ssh_command(ssh, parts.join(" "))
            }
        }
    }

    /// Run git with `stdin` piped in (e.g. `git apply -`, `git commit -F -`),
    /// returning the full `Output`. Works over ssh (ssh forwards stdin).
    pub fn git_with_stdin(
        &self,
        envs: &[(&str, &str)],
        args: &[&str],
        stdin: &[u8],
    ) -> std::io::Result<std::process::Output> {
        use std::io::Write;
        let mut cmd = self.git_command_env(envs, args);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(mut sink) = child.stdin.take() {
            // A dead child (bad args) closes the pipe; the wait below reports it.
            let _ = sink.write_all(stdin);
        }
        child.wait_with_output()
    }

    /// Resolve a path inside the repo's private gitdir via
    /// `git rev-parse --git-path` — never a literal `.git/…`, which breaks in
    /// linked worktrees where `.git` is a redirect file.
    fn resolve_git_path(&self, rel: &str) -> Option<String> {
        let p = self.git_out(&["rev-parse", "--git-path", rel])?;
        // rev-parse may answer relative to the worktree; anchor it.
        if p.starts_with('/') {
            Some(p)
        } else {
            Some(format!("{}/{p}", self.path()))
        }
    }

    /// Read a file inside the gitdir (e.g. `rebase-merge/git-rebase-todo`,
    /// `BISECT_LOG`). `None` when absent or unreadable.
    pub fn read_git_path(&self, rel: &str) -> Option<Vec<u8>> {
        let p = self.resolve_git_path(rel)?;
        match self {
            GitLoc::Local(_) => std::fs::read(p).ok(),
            GitLoc::Remote { ssh, .. } => {
                let out = self
                    .ssh_command(ssh, format!("cat {}", util::sh_quote(&p)))
                    .output()
                    .ok()?;
                out.status.success().then_some(out.stdout)
            }
        }
    }

    /// Write a file inside the gitdir (e.g. a prepared rebase todo).
    pub fn write_git_path(&self, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
        let p = self
            .resolve_git_path(rel)
            .ok_or_else(|| std::io::Error::other(format!("cannot resolve git path {rel:?}")))?;
        match self {
            GitLoc::Local(_) => std::fs::write(p, bytes),
            GitLoc::Remote { ssh, .. } => {
                use std::io::Write;
                let mut cmd = self.ssh_command(ssh, format!("cat > {}", util::sh_quote(&p)));
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                let mut child = cmd.spawn()?;
                if let Some(mut sink) = child.stdin.take() {
                    sink.write_all(bytes)?;
                }
                let st = child.wait()?;
                if st.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "remote write of {rel:?} failed"
                    )))
                }
            }
        }
    }

    /// Run a git command, returning trimmed stdout on success (None otherwise).
    pub fn git_out(&self, args: &[&str]) -> Option<String> {
        let out = self.git_command(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    /// Run a git command for its exit status (output discarded).
    pub fn git_ok(&self, args: &[&str]) -> bool {
        self.git_command(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_roundtrip() {
        let loc = GitLoc::from_db("/wt/x", None);
        assert!(!loc.is_remote());
        assert_eq!(loc.path(), "/wt/x");
    }

    #[test]
    fn remote_roundtrip_and_argv() {
        let s = GitLoc::remote_db_string("user@box", 2222, true, "/remote/wt");
        let loc = GitLoc::from_db("/ignored", Some(&s));
        assert!(loc.is_remote());
        assert_eq!(loc.path(), "/remote/wt");
        // git_command builds an ssh invocation carrying the remote git command.
        let cmd = loc.git_command(&["status", "--short"]);
        let argv: Vec<String> = std::iter::once(cmd.get_program().to_string_lossy().into_owned())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|a| a == "-p"));
        assert!(argv.iter().any(|a| a == "user@box"));
        assert!(
            argv.last()
                .unwrap()
                .contains("git -C /remote/wt status --short")
        );
    }

    #[test]
    fn env_command_prefixes_env_remotely_and_sets_it_locally() {
        let s = GitLoc::remote_db_string("box", 22, false, "/r/wt");
        let remote = GitLoc::from_db("/r/wt", Some(&s));
        let cmd = remote.git_command_env(&[("GIT_EDITOR", ":"), ("X", "a b")], &["rebase", "-i"]);
        let last = cmd
            .get_args()
            .last()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            last.starts_with("env GIT_EDITOR=: X='a b' git -C /r/wt rebase -i"),
            "{last}"
        );

        let local = GitLoc::from_db("/wt/x", None);
        let cmd = local.git_command_env(&[("GIT_EDITOR", ":")], &["rebase", "-i"]);
        assert_eq!(cmd.get_program().to_string_lossy(), "git");
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k.to_string_lossy() == "GIT_EDITOR"
                    && v.is_some_and(|v| v.to_string_lossy() == ":"))
        );
    }

    #[test]
    fn local_git_commands_scrub_repo_targeting_env() {
        // The local svc git layer must strip inherited GIT_DIR/GIT_WORK_TREE/…
        // so a poisoned ambient env can't make a `-C <worktree>` op write a
        // stray core.worktree into the shared `.git/config` (the pollution bug).
        let loc = GitLoc::from_db("/wt/x", None);
        for builder in [
            loc.git_command(&["status"]),
            loc.git_command_env(&[("GIT_EDITOR", ":")], &["rebase", "-i"]),
        ] {
            let removed: Vec<String> = builder
                .get_envs()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
            for var in crate::util::GIT_ENV_VARS {
                assert!(removed.contains(&var.to_string()), "{var} not scrubbed");
            }
        }
    }

    #[test]
    fn git_with_stdin_pipes_bytes_through() {
        // `git hash-object --stdin` works without a repo and echoes a stable
        // sha for known input — a hermetic stdin round-trip.
        let loc = GitLoc::Local(std::env::temp_dir());
        let out = loc
            .git_with_stdin(&[], &["hash-object", "--stdin"], b"hello\n")
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn gh_command_cds_remote() {
        let s = GitLoc::remote_db_string("box", 22, false, "/r/wt");
        let loc = GitLoc::from_db("/r/wt", Some(&s));
        let cmd = loc.gh_command(&["pr", "view"]);
        let last = cmd
            .get_args()
            .last()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(last.contains("cd /r/wt && gh pr view"));
    }
}
