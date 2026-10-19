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

use crate::store::WorkspaceStore;
use crate::util;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Sequencer files and BISECT_LOG are tiny; cap remote/provider capture so a
/// hostile or corrupted gitdir cannot stream unbounded data into host memory.
const GIT_PATH_READ_LIMIT: usize = 8 * 1024 * 1024;
/// Bound conditional git-path rewrites to the same wait used by svc mutations.
const GIT_PATH_LOCK_WAIT: Duration = Duration::from_secs(120);
static GIT_PATH_WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub forward_agent: bool,
    /// `-F` — an ssh_config to read instead of the user's.
    ///
    /// The three knobs below exist because a *local VM* is reachable only with
    /// them: `podman machine` writes a generated config and a dedicated key
    /// (`~/.ssh/podman-machine-default`), and regenerates the VM's host key on
    /// every `init`, so a bare `ssh -p <port> host` fails on host-key or auth
    /// before it ever runs a command. [`crate::placement::SshPlacement`] already
    /// carried them; `SshTarget` did not, so every control-plane read built its
    /// argv from host/port/forward_agent alone and silently dropped the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_config: Option<String>,
    /// `-J` — a jump host to reach this target through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_host: Option<String>,
    /// `-i` — the identity file to authenticate with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Verbatim extra `ssh` arguments, appended last so they can override.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl SshTarget {
    /// A target with no ssh knobs — an ordinary reachable host.
    ///
    /// Mirrors [`crate::placement::SshPlacement::plain`]. Deliberately not a
    /// `Default` impl: the default port would be 0, and a silently unreachable
    /// target is exactly the failure this type's knobs exist to prevent.
    pub fn plain(host: String, port: u16, forward_agent: bool) -> Self {
        Self {
            host,
            port,
            forward_agent,
            ssh_config: None,
            jump_host: None,
            identity: None,
            extra_args: Vec::new(),
        }
    }

    /// The ssh argv prefix for this target, **including** its config, jump host,
    /// identity and extra args.
    ///
    /// Every control-plane call site must go through this rather than calling
    /// [`ssh_base`] with the bare triple: the knobs are not decoration, they are
    /// what makes a target reachable at all. The ordering matches
    /// [`crate::placement::SshPlacement::ssh_base`], so a placement and a target
    /// built from the same host produce the same command.
    pub fn ssh_base(&self, batch: bool) -> Vec<String> {
        let mut v = ssh_base(self.port, self.forward_agent, batch);
        if let Some(cfg) = &self.ssh_config {
            v.extend(["-F".to_string(), cfg.clone()]);
        }
        if let Some(j) = &self.jump_host {
            v.extend(["-J".to_string(), j.clone()]);
        }
        if let Some(id) = &self.identity {
            v.extend(["-i".to_string(), id.clone()]);
        }
        v.extend(self.extra_args.iter().cloned());
        v
    }
}

/// The ssh argv prefix (without the host) for `port`, with ControlMaster
/// multiplexing so the panel's frequent git polls reuse one connection.
///
/// `batch` is for the *control plane* (git/gh reads, container lifecycle): it adds
/// `BatchMode=yes` so a missing key fails fast instead of prompting for a password
/// on a captured (non-interactive) channel and stealing the pane's stdin. The
/// interactive pane (mosh/ssh) passes `batch = false` so auth prompts still work.
pub fn ssh_base(port: u16, forward_agent: bool, batch: bool) -> Vec<String> {
    let tune = crate::remote_tune::ssh_tune();
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
    v.push(format!("ConnectTimeout={}", tune.connect_timeout_secs));
    // Keepalives so a lossy link (cellular, flappy Tailscale relay) is detected
    // as *dead* by ssh in ~interval×count seconds instead of hanging a
    // control-plane exec until the caller's deadline kills it — and so a
    // healthy-but-quiet multiplexed master isn't dropped by a NAT timeout.
    v.extend(crate::remote_tune::keepalive_args(tune));
    // Multiplex so the panel's frequent git polls reuse one connection (and the
    // interactive pane's master serves later control-plane calls without re-auth).
    // The ControlPath's parent (`<thegn_dir>/run`) is created by the compositor
    // at startup, but a headless CLI (`thegn host provision`, `pr`, `diff`, …)
    // can run before that ever happens — ensure it once so ssh can bind the socket
    // (otherwise ControlMaster fails with "cannot bind to path … No such file").
    {
        static RUN_DIR_ONCE: std::sync::Once = std::sync::Once::new();
        let rd = util::thegn_dir().join("run");
        // best-effort: if the mkdir fails, ssh reports the bind error as before.
        RUN_DIR_ONCE.call_once(move || {
            let _ = std::fs::create_dir_all(rd);
        });
    }
    v.push("-o".into());
    v.push("ControlMaster=auto".into());
    v.push("-o".into());
    v.push(format!("ControlPath={}", control_path_template().display()));
    v.push("-o".into());
    v.push(format!("ControlPersist={}", tune.control_persist_secs));
    v
}

/// The ControlPath *template* handed to ssh (`%r@%h:%p` placeholders intact).
fn control_path_template() -> PathBuf {
    util::thegn_dir().join("run").join("ssh-%r@%h:%p")
}

/// The concrete ControlMaster socket path for a `user@host` + port pair —
/// the expansion of the `ssh-%r@%h:%p` template ssh binds. Used by master
/// hygiene (`ssh -O check` / stale-socket unlink); must stay in lockstep with
/// [`ssh_base`]'s ControlPath.
///
/// `host` may be `user@host` or a bare host; ssh expands `%r` to the *remote
/// user*, which for a bare host is the local username — matching ssh's own
/// expansion there would require resolving ssh_config, so callers pass the
/// same `user@host` string they hand to ssh and we mirror the common case.
pub fn control_path(host: &str, port: u16) -> PathBuf {
    let (user, bare_host) = match host.split_once('@') {
        Some((u, h)) => (u.to_string(), h),
        None => (std::env::var("USER").unwrap_or_default(), host),
    };
    util::thegn_dir()
        .join("run")
        .join(format!("ssh-{user}@{bare_host}:{port}"))
}

/// Resolve the remote `$HOME` over ssh, so we can store absolute remote paths
/// (a `~` would not survive the shell-quoting in the git shim).
pub fn remote_home(ssh: &SshTarget) -> Option<String> {
    let mut argv = ssh.ssh_base(true);
    argv.push(ssh.host.clone());
    argv.push("printf %s \"$HOME\"".into());
    let out = Command::new(&argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Serialized form stored in `worktrees.location` for an ssh remote.
///
/// The ssh knobs are `#[serde(default, skip_serializing_if …)]` so a row written
/// before they existed still deserializes and a target that uses none of them
/// still serializes to the same three-field JSON — the location string is
/// compared and rewritten, so churning it would be a needless DB write on every
/// hydrate.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteLoc {
    host: String,
    port: u16,
    #[serde(default)]
    forward_agent: bool,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssh_config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jump_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extra_args: Vec<String>,
}

/// Serialized form stored in `worktrees.location` for a managed-provider
/// (e.g. Sprites) worktree: the control-plane exec prefix that runs a command
/// *inside* the environment, plus the worktree's path inside it. Distinguished
/// from [`RemoteLoc`] by the required `control_prefix` field (no `host`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderLoc {
    control_prefix: Vec<String>,
    path: String,
}

/// Where a worktree's git data lives — local on disk, on a remote over ssh, or
/// inside a managed-provider sandbox reached via a control-plane exec prefix.
/// All three drive the same control-plane git/gh/cli/sh reads the chrome shows.
#[derive(Debug, Clone)]
pub enum GitLoc {
    Local(PathBuf),
    Remote {
        ssh: SshTarget,
        path: String,
    },
    /// `control_prefix` runs a command in the env, e.g.
    /// `["sprite","exec","-s","<name>","--"]`; `path` is the worktree dir inside
    /// the env (e.g. `/workspace`). Built from a `Placement::Provider`.
    Provider {
        control_prefix: Vec<String>,
        path: String,
    },
}

impl GitLoc {
    /// Resolve a worktree location without treating a DB failure as local.
    /// A genuinely absent row still means a local worktree.
    #[track_caller]
    pub fn for_worktree_checked(path: &Path) -> Result<GitLoc, String> {
        let p = path
            .to_str()
            .ok_or_else(|| "worktree path is not valid UTF-8".to_string())?;
        let db = crate::db::Db::open().map_err(|e| format!("open worktree location DB: {e}"))?;
        let loc = db
            .location_for(p)
            .map_err(|e| format!("read worktree location: {e}"));
        Self::from_checked_lookup(p, loc)
    }

    fn from_checked_lookup(
        path: &str,
        location: Result<Option<String>, String>,
    ) -> Result<GitLoc, String> {
        match location {
            Ok(Some(location)) if !location.trim().is_empty() && location.trim() != "local" => {
                let parsed = Self::from_db(path, Some(location.as_str()));
                if matches!(&parsed, GitLoc::Local(_)) {
                    return Err("unrecognised worktree location; refusing terminal launch".into());
                }
                Ok(parsed)
            }
            Ok(location) => Ok(Self::from_db(path, location.as_deref())),
            Err(error) => Err(error),
        }
    }

    /// Resolve a worktree path's location from the DB. Local on any miss, so
    /// existing/local worktrees behave exactly as before.
    #[track_caller]
    pub fn for_worktree(path: &Path) -> GitLoc {
        let p = path.to_string_lossy().into_owned();
        let loc = crate::db::Db::open()
            .ok()
            .and_then(|db| db.location_for(&p).ok().flatten());
        Self::from_db(&p, loc.as_deref())
    }
}

/// Worktree path to persisted `location`, built from rows already loaded so
/// hot loops can resolve locations without opening the database per row. A path
/// with no row resolves local, as [`GitLoc::for_worktree`] does on a DB miss.
#[derive(Debug, Clone, Default)]
pub struct WorktreeLocations {
    by_path: std::collections::HashMap<String, String>,
}

impl WorktreeLocations {
    /// Build an index from worktree rows, skipping rows without a path.
    pub fn from_rows(rows: &[crate::models::WorktreeRow]) -> Self {
        Self::from_pairs(
            rows.iter()
                .map(|row| (row.worktree.as_str(), row.location.as_str())),
        )
    }

    /// Build an index from path and location pairs, skipping empty paths.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let by_path = pairs
            .into_iter()
            .filter(|(path, _)| !path.is_empty())
            .map(|(path, location)| (path.to_owned(), location.to_owned()))
            .collect();
        Self { by_path }
    }

    /// Resolve a path using the indexed DB location, or local on a miss.
    pub fn loc(&self, path: &Path) -> GitLoc {
        let path = path.to_string_lossy().into_owned();
        GitLoc::from_db(&path, self.by_path.get(&path).map(String::as_str))
    }
}

impl GitLoc {
    /// Build from a worktree path + its DB `location` column value. Tries the
    /// provider blob first (has `control_prefix`), then the ssh blob (has
    /// `host`), then falls back to local — so existing local/ssh entries are
    /// unchanged.
    pub fn from_db(path: &str, location: Option<&str>) -> GitLoc {
        let Some(json) = location
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "local")
        else {
            return GitLoc::Local(PathBuf::from(path));
        };
        if let Ok(p) = serde_json::from_str::<ProviderLoc>(json)
            && !p.control_prefix.is_empty()
        {
            return GitLoc::Provider {
                control_prefix: p.control_prefix,
                path: p.path,
            };
        }
        match serde_json::from_str::<RemoteLoc>(json) {
            Ok(r) => GitLoc::Remote {
                ssh: SshTarget {
                    host: r.host,
                    port: r.port,
                    forward_agent: r.forward_agent,
                    ssh_config: r.ssh_config,
                    jump_host: r.jump_host,
                    identity: r.identity,
                    extra_args: r.extra_args,
                },
                path: r.path,
            },
            Err(_) => GitLoc::Local(PathBuf::from(path)),
        }
    }

    /// Build an interactive terminal process projection for this location.
    /// Remote commands use ssh's interactive authentication mode and a single
    /// quoted remote shell command. Providers have no interactive prefix in
    /// their persisted location record and must fail closed.
    pub fn interactive_argv(
        &self,
        program: &[String],
    ) -> Result<(Vec<String>, Option<PathBuf>), String> {
        match self {
            GitLoc::Local(path) => {
                if !path.is_dir() {
                    return Err(format!(
                        "worktree directory is unavailable: {}",
                        path.display()
                    ));
                }
                Ok((program.to_vec(), Some(path.clone())))
            }
            GitLoc::Remote { ssh, path } => {
                if program.is_empty() {
                    return Err("custom terminal command is empty".into());
                }
                let host = checked_ssh_destination(&ssh.host)?;
                let remote = format!(
                    "{} cd {} && exec {}",
                    util::GIT_ENV_VARS
                        .iter()
                        .map(|var| format!("unset {var};"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    util::sh_quote(path),
                    util::sh_join(program),
                );
                let mut argv = ssh.ssh_base(false);
                argv.push("-t".into());
                argv.push(host.to_string());
                argv.push("--".into());
                argv.push(util::sh_join(&["/bin/sh".into(), "-lc".into(), remote]));
                Ok((argv, None))
            }
            GitLoc::Provider { .. } => Err(
                "terminal output is not supported for provider worktrees; use output = \"popup\""
                    .into(),
            ),
        }
    }

    /// The DB `location` string for a remote worktree (`None` => store local).
    ///
    /// Kept as the bare-triple convenience; anything holding a real
    /// [`SshTarget`] must use [`GitLoc::remote_db_string_for`] so the ssh knobs
    /// are persisted — without them a target that needs `-i`/`-F` (every local
    /// VM) deserializes into one that cannot be reached.
    pub fn remote_db_string(host: &str, port: u16, forward_agent: bool, path: &str) -> String {
        Self::remote_db_string_for(
            &SshTarget {
                host: host.into(),
                port,
                forward_agent,
                ssh_config: None,
                jump_host: None,
                identity: None,
                extra_args: Vec::new(),
            },
            path,
        )
    }

    /// The DB `location` string for a remote worktree, carrying every ssh knob.
    pub fn remote_db_string_for(ssh: &SshTarget, path: &str) -> String {
        serde_json::to_string(&RemoteLoc {
            host: ssh.host.clone(),
            port: ssh.port,
            forward_agent: ssh.forward_agent,
            path: path.into(),
            ssh_config: ssh.ssh_config.clone(),
            jump_host: ssh.jump_host.clone(),
            identity: ssh.identity.clone(),
            extra_args: ssh.extra_args.clone(),
        })
        .unwrap_or_default()
    }

    /// The DB `location` string for a managed-provider worktree.
    pub fn provider_db_string(control_prefix: &[String], path: &str) -> String {
        serde_json::to_string(&ProviderLoc {
            control_prefix: control_prefix.to_vec(),
            path: path.into(),
        })
        .unwrap_or_default()
    }

    /// Construct a provider location directly (the worktree dir lives inside the
    /// env; `control_prefix` runs a command there).
    pub fn provider(control_prefix: Vec<String>, path: impl Into<String>) -> GitLoc {
        GitLoc::Provider {
            control_prefix,
            path: path.into(),
        }
    }

    /// Whether the worktree's git data lives off the local host (ssh OR provider)
    /// — i.e. control-plane reads must be wrapped, and gix-native reads delegate
    /// to the CLI fallback.
    pub fn is_remote(&self) -> bool {
        !matches!(self, GitLoc::Local(_))
    }

    pub fn ssh(&self) -> Option<&SshTarget> {
        match self {
            GitLoc::Remote { ssh, .. } => Some(ssh),
            GitLoc::Local(_) | GitLoc::Provider { .. } => None,
        }
    }

    /// The worktree path (local path, or the absolute path inside the remote/env).
    pub fn path(&self) -> String {
        match self {
            GitLoc::Local(p) => p.to_string_lossy().into_owned(),
            GitLoc::Remote { path, .. } => path.clone(),
            GitLoc::Provider { path, .. } => path.clone(),
        }
    }

    /// The per-worktree DB cache key: the **host-side** worktree path. Never
    /// key a cache by [`GitLoc::path()`] — for provider/remote placements that
    /// is the path *inside* the sandbox (e.g. `/workspace`), identical for
    /// every sandboxed worktree, so all their cache rows would collide into
    /// one. An associated function (not a method) because `GitLoc` does not
    /// retain the host path it was resolved from.
    pub fn worktree_cache_key(worktree: &Path) -> String {
        worktree.to_string_lossy().into_owned()
    }

    /// Best-effort `rm -rf` of THIS worktree's own directory on the box it lives
    /// on (ssh remote or provider env) — the local delete path can't reach a
    /// remote checkout, so it would otherwise leak. ONLY the worktree dir is
    /// removed; the shared base image + warm volumes (labelled
    /// `thegn.managed`) are untouched. No-op for a local loc (the caller
    /// removes that itself) or an empty/`~`/`/` path (never `rm -rf /`).
    pub fn remove_remote_dir(&self) {
        let path = self.path();
        let p = path.trim();
        if p.is_empty() || p == "/" || p == "~" {
            return;
        }
        let script = format!("rm -rf {}", util::sh_quote(p));
        // best-effort: a lingering remote dir is reclaimable via `host rm-cache`.
        let _ = match self {
            GitLoc::Local(_) => return,
            GitLoc::Remote { ssh, .. } => self.ssh_command(ssh, script).output(),
            GitLoc::Provider { control_prefix, .. } => {
                self.provider_command(control_prefix, script).output()
            }
        };
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
            GitLoc::Provider {
                control_prefix,
                path,
            } => self.provider_command(control_prefix, {
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
            GitLoc::Provider {
                control_prefix,
                path,
            } => {
                let mut gh = vec!["gh".to_string()];
                gh.extend(args.iter().map(|s| s.to_string()));
                let remote = format!("cd {} && {}", util::sh_quote(path), util::sh_join(&gh));
                self.provider_command(control_prefix, remote)
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
            GitLoc::Provider {
                control_prefix,
                path,
            } => {
                let mut argv = vec![bin.to_string()];
                argv.extend(args.iter().map(|s| s.to_string()));
                let remote = format!("cd {} && {}", util::sh_quote(path), util::sh_join(&argv));
                self.provider_command(control_prefix, remote)
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
            GitLoc::Provider {
                control_prefix,
                path,
            } => self.provider_command(
                control_prefix,
                format!("cd {} && {script}", util::sh_quote(path)),
            ),
        }
    }

    fn ssh_command(&self, ssh: &SshTarget, remote_cmd: String) -> Command {
        let mut argv = ssh.ssh_base(true);
        argv.push(ssh.host.clone());
        argv.push(remote_cmd);
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    }

    /// Wrap a shell script to run *inside* a managed-provider env via its
    /// control-plane exec prefix, e.g. `sprite exec -s <name> -- /bin/sh -lc
    /// "<script>"`. The provider/transport-agnostic sibling of
    /// [`ssh_command`](Self::ssh_command).
    fn provider_command(&self, control_prefix: &[String], script: String) -> Command {
        let mut argv = control_prefix.to_vec();
        argv.push("/bin/sh".into());
        argv.push("-lc".into());
        argv.push(script);
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
                self.ssh_command(ssh, Self::env_git_script(envs, path, args))
            }
            GitLoc::Provider {
                control_prefix,
                path,
            } => self.provider_command(control_prefix, Self::env_git_script(envs, path, args)),
        }
    }

    /// `env K=V … git -C <path> <args>` as one sh-quoted shell string (the
    /// remote/provider form of [`git_command_env`](Self::git_command_env)).
    fn env_git_script(envs: &[(&str, &str)], path: &str, args: &[&str]) -> String {
        let mut parts = vec!["env".to_string()];
        for (k, v) in envs {
            parts.push(format!("{k}={}", util::sh_quote(v)));
        }
        parts.push("git".into());
        parts.push("-C".into());
        parts.push(util::sh_quote(path));
        parts.extend(args.iter().map(|s| util::sh_quote(s)));
        parts.join(" ")
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
    /// `BISECT_LOG`). Best-effort compatibility wrapper: `None` when absent
    /// or unreadable.
    pub fn read_git_path(&self, rel: &str) -> Option<Vec<u8>> {
        // best-effort: legacy callers intentionally treat unavailable metadata as absent.
        self.read_git_path_checked(rel).ok().flatten()
    }

    /// Read a gitdir file while distinguishing absence from errors. Remote
    /// reads prefix the payload with a one-byte presence tag so transport or
    /// command failures cannot be mistaken for a missing file.
    pub fn read_git_path_checked(&self, rel: &str) -> std::io::Result<Option<Vec<u8>>> {
        self.read_git_path_checked_limited(rel, GIT_PATH_READ_LIMIT)
    }

    fn read_git_path_checked_limited(
        &self,
        rel: &str,
        limit: usize,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let p = self
            .resolve_git_path(rel)
            .ok_or_else(|| std::io::Error::other(format!("cannot resolve git path {rel:?}")))?;
        match self {
            GitLoc::Local(_) => match std::fs::read(&p) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::symlink_metadata(&p) {
                        Ok(_) => Err(error),
                        Err(metadata_error)
                            if metadata_error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            Ok(None)
                        }
                        Err(metadata_error) => Err(metadata_error),
                    }
                }
                Err(error) => Err(error),
            },
            GitLoc::Remote { ssh, .. } => {
                let script = tagged_git_path_read_ssh_command(&p);
                read_tagged_bounded(self.ssh_command(ssh, script), rel, limit)
            }
            GitLoc::Provider { control_prefix, .. } => {
                let script = tagged_git_path_read_script(&p);
                read_tagged_bounded(self.provider_command(control_prefix, script), rel, limit)
            }
        }
    }

    /// Write a file inside the gitdir (e.g. a prepared rebase todo).
    pub fn write_git_path(&self, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
        let p = self
            .resolve_git_path(rel)
            .ok_or_else(|| std::io::Error::other(format!("cannot resolve git path {rel:?}")))?;
        if let GitLoc::Local(_) = self {
            let path = Path::new(&p);
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(std::io::Error::other(format!(
                        "refusing to write git path {rel:?} through a symlink"
                    )));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let nonce = GIT_PATH_WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
            let temporary =
                PathBuf::from(format!("{}.thegn-tmp.{}.{}", p, std::process::id(), nonce));
            crate::fsperm::write_owner_only_new(&temporary, bytes)?;
            let result = crate::fsperm::rename_replace(&temporary, path);
            if let Err(error) = &result {
                tracing::debug!(target: "thegn::remote", %error, "atomic git path replacement failed");
                match std::fs::remove_file(&temporary) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::debug!(target: "thegn::remote", %error, "could not remove temporary git path write");
                    }
                }
            }
            return result;
        }
        let script = atomic_git_path_write_script(&p);
        let mut cmd = match self {
            GitLoc::Remote { ssh, .. } => {
                self.ssh_command(ssh, atomic_git_path_write_ssh_command(&p))
            }
            GitLoc::Provider { control_prefix, .. } => {
                self.provider_command(control_prefix, script)
            }
            GitLoc::Local(_) => unreachable!("handled above"),
        };
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn()?;
        let write_result = match child.stdin.take() {
            Some(mut sink) => std::io::Write::write_all(&mut sink, bytes),
            None => Err(std::io::Error::other("git path writer has no stdin pipe")),
        };
        if let Err(error) = write_result {
            reap_child(&mut child);
            return Err(error);
        }
        let st = match child.wait() {
            Ok(status) => status,
            Err(error) => {
                reap_child(&mut child);
                return Err(error);
            }
        };
        if st.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "remote write of {rel:?} failed"
            )))
        }
    }

    /// Replace a gitdir file only while its exact current bytes still match
    /// `expected`. The file must already exist. This takes the bounded
    /// git-mutation lock and callers must not already hold it. The lock
    /// excludes other thegn writers only; git takes no lock on the todo, so an
    /// external editor can still race between check and rename.
    pub fn write_git_path_if_unchanged(
        &self,
        rel: &str,
        expected: &[u8],
        bytes: &[u8],
    ) -> std::io::Result<()> {
        self.write_git_path_if_unchanged_within(rel, expected, bytes, GIT_PATH_LOCK_WAIT)
    }

    fn write_git_path_if_unchanged_within(
        &self,
        rel: &str,
        expected: &[u8],
        bytes: &[u8],
        wait: Duration,
    ) -> std::io::Result<()> {
        let p = self
            .resolve_git_path(rel)
            .ok_or_else(|| std::io::Error::other(format!("cannot resolve git path {rel:?}")))?;
        if let GitLoc::Local(_) = self {
            let _lock = crate::util::try_lock_git_mutations_within(Path::new(&self.path()), wait)
                .map_err(|error| {
                std::io::Error::other(format!(
                    "refusing to rewrite git path {rel:?}: git-mutation lock unavailable: {error}"
                ))
            })?;
            let path = Path::new(&p);
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(std::io::Error::other(format!(
                        "refusing to write git path {rel:?} through a symlink"
                    )));
                }
                Ok(_) => {}
                Err(error) => return Err(error),
            }
            let nonce = GIT_PATH_WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
            let temporary =
                PathBuf::from(format!("{}.thegn-tmp.{}.{}", p, std::process::id(), nonce));
            crate::fsperm::write_owner_only_new(&temporary, bytes)?;
            let result = (|| {
                let mut current = crate::fsperm::open_regular_file_nofollow(path)?;
                let mut current_bytes = Vec::new();
                std::io::Read::read_to_end(&mut current, &mut current_bytes)?;
                if current_bytes != expected {
                    return Err(std::io::Error::other(format!(
                        "git path {rel:?} changed on disk"
                    )));
                }
                crate::fsperm::rename_replace(&temporary, path)
            })();
            if let Err(error) = &result {
                tracing::debug!(target: "thegn::remote", %error, "conditional git path replacement failed");
                match std::fs::remove_file(&temporary) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::debug!(target: "thegn::remote", %error, "could not remove temporary conditional git path write");
                    }
                }
            }
            return result;
        }

        let expected_len = expected.len();
        let common_dir = self
            .git_out(&["rev-parse", "--git-common-dir"])
            .ok_or_else(|| {
                std::io::Error::other("cannot resolve git common dir for mutation lock")
            })?;
        let common_dir = Path::new(&common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir.to_path_buf()
        } else {
            Path::new(&self.path()).join(common_dir)
        };
        let lock_path = common_dir
            .join("thegn-git.lock")
            .to_string_lossy()
            .into_owned();
        let wait_secs = wait.as_secs().max(1);
        let script = cas_git_path_write_script(&p, expected_len, &lock_path, wait_secs);
        let mut cmd = match self {
            GitLoc::Remote { ssh, .. } => self.ssh_command(
                ssh,
                cas_git_path_write_ssh_command(&p, expected_len, &lock_path, wait_secs),
            ),
            GitLoc::Provider { control_prefix, .. } => {
                self.provider_command(control_prefix, script)
            }
            GitLoc::Local(_) => unreachable!("handled above"),
        };
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn()?;
        let write_result = match child.stdin.take() {
            Some(mut sink) => {
                let mut result = std::io::Write::write_all(&mut sink, expected);
                if result.is_ok() {
                    result = std::io::Write::write_all(&mut sink, bytes);
                }
                result
            }
            None => Err(std::io::Error::other("git path writer has no stdin pipe")),
        };
        if let Err(error) = write_result {
            reap_child(&mut child);
            return Err(error);
        }
        let status = match child.wait() {
            Ok(status) => status,
            Err(error) => {
                reap_child(&mut child);
                return Err(error);
            }
        };
        if status.success() {
            Ok(())
        } else if status.code() == Some(42) {
            Err(std::io::Error::other(format!(
                "git path {rel:?} changed on disk"
            )))
        } else if status.code() == Some(43) {
            Err(std::io::Error::other(format!(
                "timed out waiting for git-mutation lock for {rel:?}"
            )))
        } else if status.code() == Some(44) {
            Err(std::io::Error::other(format!(
                "git-mutation lock unavailable for {rel:?} on the remote (flock(1) missing or lock path unsafe)"
            )))
        } else {
            Err(std::io::Error::other(format!(
                "conditional write of git path {rel:?} failed"
            )))
        }
    }

    /// Run a git command, returning trimmed stdout on success (None otherwise).
    pub fn git_out(&self, args: &[&str]) -> Option<String> {
        crate::util::note_core_git_spawn();
        let out = self.git_command(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    /// `git remote get-url origin`, trimmed; `None` when unset or unreadable.
    /// A local worktree answers from a memo keyed on every config input (see
    /// [`crate::git_memo::origin_url`]) instead of forking git per call; remote
    /// and provider locs always ask the far side.
    pub fn origin_url(&self) -> Option<String> {
        const ARGS: &[&str] = &["remote", "get-url", "origin"];
        match self {
            GitLoc::Local(p) => crate::git_memo::origin_url(p, || {
                crate::util::note_core_git_spawn();
                match self.git_command(ARGS).output() {
                    // Could not even spawn git: transient, never memoised.
                    Err(_) => (None, false),
                    Ok(out) => {
                        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        (
                            (out.status.success() && !url.is_empty()).then_some(url),
                            true,
                        )
                    }
                }
            }),
            _ => self.git_out(ARGS),
        }
    }

    /// Run a git command for its exit status (output discarded).
    pub fn git_ok(&self, args: &[&str]) -> bool {
        crate::util::note_core_git_spawn();
        self.git_command(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

fn checked_ssh_destination(host: &str) -> Result<&str, String> {
    if host.is_empty() {
        return Err("SSH destination is empty; refusing terminal launch".into());
    }
    if host.starts_with('-') {
        return Err("SSH destination may not start with '-'; refusing terminal launch".into());
    }
    if host
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(
            "SSH destination contains whitespace or control bytes; refusing terminal launch".into(),
        );
    }
    Ok(host)
}

/// The idempotent provisioning script for a fresh provider env (8-A.3): clone the
/// repo into the worktree dir on first open, no-op once it's a git repo. Designed
/// to run via [`GitLoc::sh_command`], which already `cd`s into the workdir — so
/// the clone targets `.` (the cwd). The leading `rev-parse` guard makes re-opens
/// cheap and makes this safe to run even after a `data=sync` upload (which lands
/// a `.git`, so the guard skips). When `branch` is set, check it out (creating it
/// if it doesn't exist on the remote). `origin` is the upstream URL.
pub fn provision_repo_script(origin: &str, branch: Option<&str>) -> String {
    provision_repo_script_with_submodules(origin, branch, false)
}

/// As provision_repo_script, optionally initialize direct submodules after
/// the clone/branch checkout. Nested initialization stays disabled until
/// THE-385's closure approval exists. The command is only emitted when the
/// host has already applied the effective policy and trust decision; this
/// function remains pure and only produces shell-quoted text.
pub fn provision_repo_script_with_submodules(
    origin: &str,
    branch: Option<&str>,
    direct_submodules: bool,
) -> String {
    let oq = util::sh_quote(origin);
    // Already a repo → done. Else clone into the cwd, then settle the branch.
    let mut s =
        String::from("if git rev-parse --git-dir >/dev/null 2>&1; then exit 0; fi; git clone ");
    s.push_str(&oq);
    s.push_str(" .");
    if let Some(b) = branch.map(str::trim).filter(|b| !b.is_empty()) {
        let bq = util::sh_quote(b);
        s.push_str(&format!(
            " && (git checkout {bq} 2>/dev/null || git checkout -b {bq})"
        ));
    }
    if direct_submodules {
        s.push_str(" && git -c submodule.recurse=false submodule update --init");
    }
    s
}

fn tagged_git_path_read_script(path: &str) -> String {
    let quoted = util::sh_quote(path);
    format!(
        "if [ -e {quoted} ] || [ -L {quoted} ]; then printf P; exec cat -- {quoted}; else printf A; fi"
    )
}

fn tagged_git_path_read_ssh_command(path: &str) -> String {
    let script = tagged_git_path_read_script(path);
    format!("sh -c {}", util::sh_quote(&script))
}

fn atomic_git_path_write_script(path: &str) -> String {
    let quoted = util::sh_quote(path);
    format!(
        "set -C; umask 077; p={quoted}; t=\"$p.thegn-tmp.$$\"; \
         [ ! -L \"$p\" ] && [ ! -d \"$p\" ] || exit 1; \
         created=0; trap 'if [ \"$created\" = 1 ]; then rm -f -- \"$t\"; fi' 0; \
         trap 'exit 1' HUP INT TERM; : > \"$t\" || exit 1; created=1; \
         set +C; cat > \"$t\" && mv -f -- \"$t\" \"$p\""
    )
}

fn atomic_git_path_write_ssh_command(path: &str) -> String {
    let script = atomic_git_path_write_script(path);
    format!("sh -c {}", util::sh_quote(&script))
}

fn cas_git_path_write_script(
    path: &str,
    expected_len: usize,
    lock_path: &str,
    wait_secs: u64,
) -> String {
    let quoted = util::sh_quote(path);
    let lock = util::sh_quote(lock_path);
    format!(
        "set -C; umask 077; l={lock}; command -v flock >/dev/null 2>&1 || exit 44; [ ! -L \"$l\" ] || exit 44; exec 9>>\"$l\" || exit 44; [ ! -L \"$l\" ] && [ \"$l\" -ef /dev/fd/9 ] || exit 44; flock -w {wait_secs} 9 || exit 43; p={quoted}; e=\"$p.thegn-cas.$$\"; t=\"$p.thegn-tmp.$$\"; \
         [ ! -L \"$p\" ] && [ ! -d \"$p\" ] && [ -e \"$p\" ] || exit 1; \
         ce=0; ct=0; trap 'if [ \"$ce\" = 1 ]; then rm -f -- \"$e\"; fi; if [ \"$ct\" = 1 ]; then rm -f -- \"$t\"; fi' 0; \
         trap 'exit 1' HUP INT TERM; : > \"$e\" || exit 1; ce=1; : > \"$t\" || exit 1; ct=1; \
         set +C; dd bs=1 count={expected_len} 2>/dev/null > \"$e\" || exit 1; \
         [ \"$(wc -c < \"$e\")\" -eq {expected_len} ] || exit 1; cat > \"$t\" || exit 1; \
         cmp -s -- \"$e\" \"$p\"; cmp_status=$?; \
         if [ \"$cmp_status\" -eq 1 ]; then exit 42; fi; \
         [ \"$cmp_status\" -eq 0 ] || exit 1; mv -f -- \"$t\" \"$p\""
    )
}

fn cas_git_path_write_ssh_command(
    path: &str,
    expected_len: usize,
    lock_path: &str,
    wait_secs: u64,
) -> String {
    let script = cas_git_path_write_script(path, expected_len, lock_path, wait_secs);
    format!("sh -c {}", util::sh_quote(&script))
}

/// Capture a tagged remote git-path read with a strict payload limit. On every
/// read/limit failure the child is killed when possible and waited before the
/// error is returned, so a stalled transport cannot leave a child behind.
fn read_tagged_bounded(
    mut command: Command,
    rel: &str,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    use std::process::Stdio;

    let capture_limit = limit
        .checked_add(2)
        .ok_or_else(|| std::io::Error::other("git path read limit is too large"))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let Some(stdout) = child.stdout.take() else {
        reap_child(&mut child);
        return Err(std::io::Error::other("git path reader has no stdout pipe"));
    };
    let mut bytes = Vec::new();
    let read_result = stdout.take(capture_limit as u64).read_to_end(&mut bytes);
    if let Err(error) = read_result {
        reap_child(&mut child);
        return Err(error);
    }
    if bytes.len() > limit.saturating_add(1) {
        reap_child(&mut child);
        return Err(std::io::Error::other(format!(
            "git path {rel:?} exceeds {limit} bytes"
        )));
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            reap_child(&mut child);
            return Err(error);
        }
    };
    tagged_git_path_read_result(
        rel,
        std::process::Output {
            status,
            stdout: bytes,
            stderr: Vec::new(),
        },
    )
}

fn reap_child(child: &mut std::process::Child) {
    if let Err(error) = child.kill() {
        tracing::debug!(target: "thegn::remote", %error, "could not kill git path reader");
    }
    if let Err(error) = child.wait() {
        tracing::debug!(target: "thegn::remote", %error, "could not reap git path reader");
    }
}

fn tagged_git_path_read_result(
    rel: &str,
    output: std::process::Output,
) -> std::io::Result<Option<Vec<u8>>> {
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "read git path {rel:?} failed with exit status {}",
            output.status
        )));
    }
    match output.stdout.split_first() {
        Some((&b'A', [])) => Ok(None),
        Some((&b'P', bytes)) => Ok(Some(bytes.to_vec())),
        _ => Err(std::io::Error::other(format!(
            "read git path {rel:?} returned an invalid presence tag"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_base_has_keepalives_and_multiplexing() {
        let joined = ssh_base(22, false, true).join(" ");
        // Keepalives (the flaky-link hardening) ride every control-plane ssh.
        assert!(joined.contains("-o ServerAliveInterval="), "{joined}");
        assert!(joined.contains("-o ServerAliveCountMax="), "{joined}");
        assert!(joined.contains("-o TCPKeepAlive=yes"), "{joined}");
        // The historical contract holds.
        assert!(joined.contains("-o BatchMode=yes"), "{joined}");
        assert!(joined.contains("-o ConnectTimeout="), "{joined}");
        assert!(joined.contains("-o ControlMaster=auto"), "{joined}");
        assert!(joined.contains("-o ControlPersist="), "{joined}");
        assert!(joined.contains("ControlPath="), "{joined}");
        assert!(joined.contains("ssh-%r@%h:%p"), "{joined}");
    }

    #[test]
    fn control_path_expands_the_template() {
        // user@host form mirrors ssh's %r@%h:%p expansion exactly.
        let p = control_path("targe@ageless-studio", 22);
        assert!(
            p.to_string_lossy()
                .ends_with("run/ssh-targe@ageless-studio:22"),
            "{}",
            p.display()
        );
        // Non-default port lands in the :%p slot.
        let p = control_path("u@h", 2222);
        assert!(
            p.to_string_lossy().ends_with("run/ssh-u@h:2222"),
            "{}",
            p.display()
        );
        // Same parent dir as the template ssh_base hands out.
        assert_eq!(p.parent(), control_path_template().parent());
    }

    #[test]
    fn local_roundtrip() {
        let loc = GitLoc::from_db("/wt/x", None);
        assert!(!loc.is_remote());
        assert_eq!(loc.path(), "/wt/x");
    }

    #[test]
    fn checked_local_read_treats_dangling_symlink_as_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let init = util::git_cmd(dir.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(init.success());
        let link = dir.path().join(".git/dangling-marker");
        if let Err(error) =
            crate::fsperm::create_symlink(std::path::Path::new("missing-target"), &link, false)
        {
            if cfg!(unix) {
                panic!("symlink fixture: {error}");
            }
            return;
        }

        let loc = GitLoc::Local(dir.path().to_path_buf());
        assert!(loc.read_git_path_checked("dangling-marker").is_err());
    }

    #[test]
    fn ssh_tagged_read_script_is_explicitly_run_by_posix_shell() {
        let command = tagged_git_path_read_ssh_command("/work tree/marker");
        let script = "if [ -e '/work tree/marker' ] || [ -L '/work tree/marker' ]; then printf P; exec cat -- '/work tree/marker'; else printf A; fi";
        assert_eq!(command, format!("sh -c {}", util::sh_quote(script)));
    }

    #[test]
    fn ssh_atomic_write_script_is_explicitly_run_by_posix_shell() {
        let command = atomic_git_path_write_ssh_command("/work tree/todo");
        let script = "set -C; umask 077; p='/work tree/todo'; t=\"$p.thegn-tmp.$$\"; [ ! -L \"$p\" ] && [ ! -d \"$p\" ] || exit 1; created=0; trap 'if [ \"$created\" = 1 ]; then rm -f -- \"$t\"; fi' 0; trap 'exit 1' HUP INT TERM; : > \"$t\" || exit 1; created=1; set +C; cat > \"$t\" && mv -f -- \"$t\" \"$p\"";
        assert_eq!(command, format!("sh -c {}", util::sh_quote(script)));
    }

    #[test]
    fn ssh_cas_write_script_is_explicitly_run_by_posix_shell() {
        let command = cas_git_path_write_ssh_command(
            "/work tree/todo",
            7,
            "/work tree/.git/thegn-git.lock",
            120,
        );
        let script = "set -C; umask 077; l='/work tree/.git/thegn-git.lock'; command -v flock >/dev/null 2>&1 || exit 44; [ ! -L \"$l\" ] || exit 44; exec 9>>\"$l\" || exit 44; [ ! -L \"$l\" ] && [ \"$l\" -ef /dev/fd/9 ] || exit 44; flock -w 120 9 || exit 43; p='/work tree/todo'; e=\"$p.thegn-cas.$$\"; t=\"$p.thegn-tmp.$$\"; [ ! -L \"$p\" ] && [ ! -d \"$p\" ] && [ -e \"$p\" ] || exit 1; ce=0; ct=0; trap 'if [ \"$ce\" = 1 ]; then rm -f -- \"$e\"; fi; if [ \"$ct\" = 1 ]; then rm -f -- \"$t\"; fi' 0; trap 'exit 1' HUP INT TERM; : > \"$e\" || exit 1; ce=1; : > \"$t\" || exit 1; ct=1; set +C; dd bs=1 count=7 2>/dev/null > \"$e\" || exit 1; [ \"$(wc -c < \"$e\")\" -eq 7 ] || exit 1; cat > \"$t\" || exit 1; cmp -s -- \"$e\" \"$p\"; cmp_status=$?; if [ \"$cmp_status\" -eq 1 ]; then exit 42; fi; [ \"$cmp_status\" -eq 0 ] || exit 1; mv -f -- \"$t\" \"$p\"";
        assert_eq!(command, format!("sh -c {}", util::sh_quote(script)));
    }

    fn provider_repo() -> (tempfile::TempDir, GitLoc) {
        let dir = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(dir.path()).unwrap();
        let init = util::git_cmd(dir.path())
            .args(["init", "--quiet"])
            .status()
            .unwrap();
        assert!(init.success());
        let loc = GitLoc::provider(vec!["env".into()], dir.path().to_string_lossy());
        (dir, loc)
    }

    fn flock_available() -> bool {
        Command::new("sh")
            .args(["-c", "command -v flock >/dev/null 2>&1"])
            .status()
            .is_ok_and(|status| status.success())
    }

    fn assert_no_cas_write_temps(gitdir: &Path) {
        let leftovers = std::fs::read_dir(gitdir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.contains(".thegn-tmp.") || name.contains(".thegn-cas.")
            })
            .count();
        assert_eq!(leftovers, 0, "temporary git path files were left behind");
    }

    #[test]
    fn provider_conditional_write_replaces_only_matching_bytes_and_cleans_temps() {
        if !flock_available() {
            return;
        }
        let (dir, loc) = provider_repo();
        let gitdir = dir.path().join(".git");
        let target = gitdir.join("cas-target");
        std::fs::write(&target, b"old").unwrap();
        loc.write_git_path_if_unchanged("cas-target", b"old", b"new value")
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new value");
        assert_no_cas_write_temps(&gitdir);
    }

    #[test]
    fn provider_conditional_write_waits_for_git_mutation_lock() {
        if !flock_available() {
            return;
        }
        let (dir, loc) = provider_repo();
        let gitdir = dir.path().join(".git");
        let target = gitdir.join("cas-target");
        std::fs::write(&target, b"old").unwrap();
        let held = util::try_lock_git_mutations(dir.path()).unwrap();
        let error = loc
            .write_git_path_if_unchanged_within(
                "cas-target",
                b"old",
                b"new",
                Duration::from_secs(1),
            )
            .expect_err("provider CAS must wait for the held mutation lock");
        assert!(error.to_string().contains("git-mutation lock"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_no_cas_write_temps(&gitdir);
        drop(held);

        loc.write_git_path_if_unchanged_within(
            "cas-target",
            b"old",
            b"new",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_no_cas_write_temps(&gitdir);
    }

    #[test]
    fn local_conditional_write_waits_for_git_mutation_lock() {
        let (dir, _) = provider_repo();
        let gitdir = dir.path().join(".git");
        let target = gitdir.join("local-lock-target");
        std::fs::write(&target, b"old").unwrap();
        let loc = GitLoc::Local(dir.path().to_path_buf());
        let held = util::try_lock_git_mutations(dir.path()).unwrap();
        let error = loc
            .write_git_path_if_unchanged_within(
                "local-lock-target",
                b"old",
                b"new",
                Duration::from_secs(1),
            )
            .expect_err("local CAS must refuse under the held mutation lock");
        assert!(error.to_string().contains("git-mutation lock"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_no_cas_write_temps(&gitdir);
        drop(held);

        loc.write_git_path_if_unchanged_within(
            "local-lock-target",
            b"old",
            b"new",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_no_cas_write_temps(&gitdir);
    }

    #[test]
    fn provider_conditional_write_rejects_stale_bytes_without_changes_or_temps() {
        if !flock_available() {
            return;
        }
        let (dir, loc) = provider_repo();
        let gitdir = dir.path().join(".git");
        let target = gitdir.join("cas-target");
        std::fs::write(&target, b"newer on disk").unwrap();
        let error = loc
            .write_git_path_if_unchanged("cas-target", b"old", b"replacement")
            .expect_err("a stale expected value must be rejected");
        assert!(error.to_string().contains("changed on disk"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"newer on disk");
        assert_no_cas_write_temps(&gitdir);
    }

    #[test]
    fn provider_conditional_write_rejects_absent_and_symlink_targets() {
        if !flock_available() {
            return;
        }
        let (dir, loc) = provider_repo();
        let gitdir = dir.path().join(".git");
        assert!(
            loc.write_git_path_if_unchanged("absent", b"old", b"replacement")
                .is_err()
        );
        let outside = dir.path().join("outside-cas");
        std::fs::write(&outside, b"protected").unwrap();
        let link = gitdir.join("cas-link");
        crate::fsperm::create_symlink(&outside, &link, false).unwrap();
        assert!(
            loc.write_git_path_if_unchanged("cas-link", b"protected", b"replacement")
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"protected");
        assert_no_cas_write_temps(&gitdir);
    }

    #[test]
    fn local_conditional_write_replaces_matching_bytes_and_rejects_stale_absent_and_symlink() {
        let (dir, _) = provider_repo();
        let gitdir = dir.path().join(".git");
        let loc = GitLoc::Local(dir.path().to_path_buf());
        let target = gitdir.join("local-cas-target");
        std::fs::write(&target, b"old").unwrap();
        loc.write_git_path_if_unchanged("local-cas-target", b"old", b"replacement")
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");

        std::fs::write(&target, b"newer on disk").unwrap();
        assert!(
            loc.write_git_path_if_unchanged("local-cas-target", b"old", b"replacement")
                .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"newer on disk");
        assert!(
            loc.write_git_path_if_unchanged("local-absent", b"old", b"replacement")
                .is_err()
        );

        let outside = dir.path().join("outside-local-cas");
        std::fs::write(&outside, b"protected").unwrap();
        let link = gitdir.join("local-cas-link");
        crate::fsperm::create_symlink(&outside, &link, false).unwrap();
        assert!(
            loc.write_git_path_if_unchanged("local-cas-link", b"protected", b"replacement")
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"protected");
        assert_no_cas_write_temps(&gitdir);
    }

    #[test]
    fn provider_checked_read_enforces_limit_and_accepts_exact_limit() {
        let (dir, loc) = provider_repo();
        let gitdir = dir.path().join(".git");
        std::fs::write(gitdir.join("bounded"), b"1234").unwrap();
        assert_eq!(
            loc.read_git_path_checked_limited("bounded", 4).unwrap(),
            Some(b"1234".to_vec())
        );
        std::fs::write(gitdir.join("bounded"), b"12345").unwrap();
        let error = loc
            .read_git_path_checked_limited("bounded", 4)
            .expect_err("an oversized git path must be rejected");
        assert!(error.to_string().contains("exceeds 4 bytes"), "{error}");
    }

    #[test]
    fn oversized_tagged_reader_is_killed_and_reaped() {
        let dir = tempfile::tempdir().unwrap();
        crate::fsperm::restrict_dir_to_owner(dir.path()).unwrap();
        let pid_path = dir.path().join("producer.pid");
        let script = format!(
            "printf '%s' \"$$\" > {}; printf P; exec yes x",
            util::sh_quote(pid_path.to_string_lossy().as_ref())
        );
        let mut command = Command::new("sh");
        command.args(["-c", script.as_str()]);
        let error =
            read_tagged_bounded(command, "oversized", 1).expect_err("oversized producer must fail");
        assert!(error.to_string().contains("exceeds 1 bytes"), "{error}");
        let pid = std::fs::read_to_string(pid_path).unwrap();
        let alive = Command::new("sh")
            .args(["-c", "kill -0 \"$1\"", "sh", pid.trim()])
            .status()
            .unwrap();
        assert!(!alive.success(), "the over-limit child is still alive");
    }

    #[test]
    fn provider_write_atomically_replaces_and_cleans_temporary_file() {
        let (dir, loc) = provider_repo();
        let gitdir = dir.path().join(".git");
        let target = gitdir.join("write-target");
        std::fs::write(&target, b"old").unwrap();
        let mut old_inode = std::fs::File::open(&target).unwrap();
        loc.write_git_path("write-target", b"replacement").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
        let mut prior_contents = Vec::new();
        std::io::Read::read_to_end(&mut old_inode, &mut prior_contents).unwrap();
        assert_eq!(
            prior_contents, b"old",
            "replacement must publish a new file instead of truncating the old inode"
        );
        let leftovers = std::fs::read_dir(gitdir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".thegn-tmp."))
            .count();
        assert_eq!(leftovers, 0, "temporary git path file was left behind");
    }

    #[test]
    fn provider_write_refuses_directory_target() {
        let (dir, loc) = provider_repo();
        let target = dir.path().join(".git/write-target");
        std::fs::create_dir(&target).unwrap();

        assert!(loc.write_git_path("write-target", b"replacement").is_err());
        assert!(target.is_dir());
        assert_eq!(std::fs::read_dir(target).unwrap().count(), 0);
    }

    #[test]
    fn local_write_uses_a_fresh_temp_name_when_pid_only_name_exists() {
        let (dir, _) = provider_repo();
        let gitdir = dir.path().join(".git");
        let target = gitdir.join("write-target");
        let stale = gitdir.join(format!("write-target.thegn-tmp.{}", std::process::id()));
        std::fs::write(&stale, b"stale temporary file").unwrap();
        let local = GitLoc::Local(dir.path().to_path_buf());

        local
            .write_git_path("write-target", b"replacement")
            .unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"replacement");
        assert_eq!(std::fs::read(stale).unwrap(), b"stale temporary file");
    }

    #[test]
    fn provider_and_local_writes_refuse_symlinks_without_touching_target() {
        let (dir, loc) = provider_repo();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"protected").unwrap();
        let remote_link = dir.path().join(".git/remote-link");
        crate::fsperm::create_symlink(&outside, &remote_link, false).unwrap();
        assert!(loc.write_git_path("remote-link", b"overwrite").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"protected");

        let local_link = dir.path().join(".git/local-link");
        crate::fsperm::create_symlink(&outside, &local_link, false).unwrap();
        let local = GitLoc::Local(dir.path().to_path_buf());
        assert!(local.write_git_path("local-link", b"overwrite").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"protected");
    }

    #[test]
    fn interactive_remote_argv_uses_tty_quoted_cwd_and_scrubbed_shell() {
        let ssh = SshTarget::plain("dev@example.test".into(), 2222, false);
        let loc = GitLoc::Remote {
            ssh: ssh.clone(),
            path: "/srv/work tree/it's-here".into(),
        };
        let program = vec!["git".into(), "status".into(), "--short".into()];
        let (argv, cwd) = loc.interactive_argv(&program).unwrap();
        let mut expected = ssh.ssh_base(false);
        expected.extend([
            "-t".into(),
            "dev@example.test".into(),
            "--".into(),
            util::sh_join(&[
                "/bin/sh".into(),
                "-lc".into(),
                format!(
                    "{} cd {} && exec git status --short",
                    util::GIT_ENV_VARS
                        .iter()
                        .map(|var| format!("unset {var};"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    util::sh_quote("/srv/work tree/it's-here")
                ),
            ]),
        ]);
        assert_eq!(argv, expected);
        assert!(!argv.contains(&"BatchMode=yes".into()));
        assert_eq!(cwd, None);
    }

    #[test]
    fn interactive_remote_argv_refuses_unsafe_ssh_destinations() {
        for host in ["", "-oProxyCommand=touch x", "a b", "host\nname"] {
            let loc = GitLoc::Remote {
                ssh: SshTarget::plain(host.into(), 22, false),
                path: "/w".into(),
            };
            assert!(
                loc.interactive_argv(&["git".into()]).is_err(),
                "unsafe SSH destination should be refused: {host:?}"
            );
        }
    }

    #[test]
    fn interactive_local_missing_directory_and_provider_fail_closed() {
        let missing =
            std::env::temp_dir().join(format!("tg-terminal-missing-{}", std::process::id()));
        assert!(
            GitLoc::Local(missing)
                .interactive_argv(&["git".into()])
                .is_err()
        );
        let provider = GitLoc::Provider {
            control_prefix: vec!["sprite".into(), "exec".into()],
            path: "/workspace".into(),
        };
        assert!(
            provider
                .interactive_argv(&["git".into()])
                .unwrap_err()
                .contains("not supported")
        );
    }

    #[test]
    fn checked_lookup_preserves_database_errors_while_absence_means_local() {
        assert!(matches!(
            GitLoc::from_checked_lookup("/wt/local", Ok(None)).unwrap(),
            GitLoc::Local(path) if path == Path::new("/wt/local")
        ));
        assert!(
            GitLoc::from_checked_lookup("/wt/local", Err("database unavailable".into())).is_err()
        );
        assert!(
            GitLoc::from_checked_lookup("/wt/local", Ok(Some("garbage".into())))
                .unwrap_err()
                .contains("unrecognised worktree location")
        );
        assert!(matches!(
            GitLoc::from_db("/wt/local", None),
            GitLoc::Local(_)
        ));
    }

    #[test]
    fn remove_remote_dir_is_safe_on_local_and_guards_dangerous_paths() {
        // A local loc never sshs anything; the caller purges the local dir.
        let dir = std::env::temp_dir().join(format!("tg-rrd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        GitLoc::Local(dir.clone()).remove_remote_dir();
        assert!(
            dir.exists(),
            "local loc must NOT be removed by remove_remote_dir"
        );
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
        // Guard: an empty / root / ~ remote path must never `rm -rf` — the
        // command is skipped entirely (constructing a Remote with such a path
        // and calling it must not attempt to run anything destructive).
        for p in ["", "/", "~"] {
            let loc = GitLoc::Remote {
                ssh: SshTarget::plain("nobody@127.0.0.1".into(), 1, false),
                path: p.to_string(),
            };
            loc.remove_remote_dir(); // returns without spawning ssh
        }
    }

    #[test]
    fn provision_script_is_idempotent_and_quotes() {
        // Guard first (re-open / post-sync no-op), then clone into the cwd `.`.
        let s = provision_repo_script("https://github.com/o/r.git", Some("feat/x"));
        assert!(
            s.starts_with("if git rev-parse --git-dir"),
            "guard first: {s}"
        );
        // sh_quote passes safe URLs/branch names through unquoted.
        assert!(
            s.contains("git clone https://github.com/o/r.git ."),
            "clone into cwd: {s}"
        );
        // Branch checkout falls back to creating it when absent on the remote.
        assert!(s.contains("git checkout feat/x 2>/dev/null || git checkout -b feat/x"));
        // A branch with a space gets single-quoted.
        assert!(provision_repo_script("u", Some("a b")).contains("git checkout 'a b'"));
        // No branch → no checkout clause.
        let s2 = provision_repo_script("git@h:o/r", None);
        assert!(!s2.contains("checkout"), "no branch ⇒ no checkout: {s2}");
        // Blank branch is treated as no branch.
        assert!(!provision_repo_script("x", Some("  ")).contains("checkout"));
    }

    #[test]
    fn provision_script_initializes_only_direct_submodules() {
        let s = provision_repo_script_with_submodules("https://x/r.git", Some("main"), true);
        assert!(s.contains("submodule update --init"));
        assert!(!s.contains("--recursive"));
        let off = provision_repo_script_with_submodules("https://x/r.git", Some("main"), false);
        assert!(!off.contains("submodule update"));
    }

    #[test]
    fn provision_script_runs_in_env_via_sh_command() {
        // A provider loc wraps the script in its control prefix + the workdir cd,
        // so the clone runs inside the env at the worktree dir.
        let loc = GitLoc::provider(
            vec!["sprite".into(), "exec".into(), "--".into()],
            "/workspace",
        );
        let script = provision_repo_script("https://x/r.git", Some("main"));
        let cmd = loc.sh_command(&script);
        let argv: Vec<String> = std::iter::once(cmd.get_program().to_string_lossy().into_owned())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(argv[0], "sprite");
        let joined = argv.join(" ");
        assert!(
            joined.contains("cd /workspace &&"),
            "cd into workdir: {joined}"
        );
        assert!(joined.contains("git clone"), "carries the clone: {joined}");
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
    fn ssh_knobs_survive_the_db_roundtrip_and_reach_the_argv() {
        // A local VM is reachable only via its generated config and key, so the
        // knobs must persist into `worktrees.location` and come back out. Losing
        // them on the round-trip is silent: the location still deserializes, the
        // worktree still looks remote, and every git read simply fails to auth.
        let mut t = SshTarget::plain("core@localhost".into(), 51234, false);
        t.identity = Some("/home/me/.ssh/podman-machine-default".into());
        t.ssh_config = Some("/home/me/.config/thegn/ssh/pm.conf".into());
        t.extra_args = vec!["-o".into(), "StrictHostKeyChecking=no".into()];

        let loc = GitLoc::from_db(
            "/ignored",
            Some(&GitLoc::remote_db_string_for(&t, "/home/core/wt")),
        );
        assert_eq!(loc.ssh(), Some(&t), "every knob round-trips");

        let cmd = loc.git_command(&["status"]);
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for pair in [
            ["-i", "/home/me/.ssh/podman-machine-default"],
            ["-F", "/home/me/.config/thegn/ssh/pm.conf"],
            ["-o", "StrictHostKeyChecking=no"],
        ] {
            assert!(argv.windows(2).any(|w| w == pair), "{pair:?} in {argv:?}");
        }
    }

    #[test]
    fn a_knobless_target_serializes_to_the_pre_existing_shape() {
        // The location string is compared before it is rewritten, so emitting new
        // keys for a target that uses none of them would rewrite every remote
        // worktree's row on the next hydrate for no change in meaning.
        let s = GitLoc::remote_db_string("box", 22, false, "/r/wt");
        for key in ["ssh_config", "jump_host", "identity", "extra_args"] {
            assert!(!s.contains(key), "{key} must not appear in {s}");
        }
        // And a row written before the fields existed still loads.
        let old = r#"{"host":"box","port":22,"forward_agent":false,"path":"/r/wt"}"#;
        assert_eq!(GitLoc::from_db("/ignored", Some(old)).path(), "/r/wt");
    }

    #[test]
    fn provider_roundtrip_and_argv() {
        // A managed-provider worktree: git runs *inside* the env via the control
        // prefix (e.g. `sprite exec -s <name> --`), worktree dir = the in-env path.
        let prefix = vec![
            "sprite".to_string(),
            "exec".into(),
            "-s".into(),
            "tgt".into(),
            "--".into(),
        ];
        let s = GitLoc::provider_db_string(&prefix, "/workspace");
        let loc = GitLoc::from_db("/ignored-host-path", Some(&s));
        assert!(loc.is_remote()); // not local → gix delegates to the CLI fallback
        assert!(loc.ssh().is_none());
        assert_eq!(loc.path(), "/workspace");

        let cmd = loc.git_command(&["status", "--porcelain"]);
        let argv: Vec<String> = std::iter::once(cmd.get_program().to_string_lossy().into_owned())
            .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect();
        // sprite exec -s tgt -- /bin/sh -lc "git -C /workspace status --porcelain"
        assert_eq!(
            &argv[..6],
            &["sprite", "exec", "-s", "tgt", "--", "/bin/sh"]
        );
        assert_eq!(argv[6], "-lc");
        assert!(
            argv[7].contains("git -C /workspace status --porcelain"),
            "script: {}",
            argv[7]
        );
        // gh/cli/sh commands cd into the worktree first.
        let gh = loc.gh_command(&["pr", "list"]);
        let gh_argv: Vec<String> = gh
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            gh_argv
                .last()
                .unwrap()
                .contains("cd /workspace && gh pr list")
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

    #[test]
    fn worktree_locations_match_gitloc_from_db_for_persisted_rows_and_misses() {
        let ssh = GitLoc::remote_db_string("box", 22, false, "/r/wt");
        let provider =
            GitLoc::provider_db_string(&["sprite".into(), "exec".into()], "/provider/wt");
        let rows = [
            crate::models::WorktreeRow {
                worktree: "/r/wt".into(),
                branch: String::new(),
                agent: String::new(),
                created_at: 0,
                repo_root: String::new(),
                tab_name: String::new(),
                session_name: String::new(),
                location: ssh.clone(),
                position: 0,
                sandbox_backend: None,
                observed_backend: None,
            },
            crate::models::WorktreeRow {
                worktree: "/provider/wt".into(),
                branch: String::new(),
                agent: String::new(),
                created_at: 0,
                repo_root: String::new(),
                tab_name: String::new(),
                session_name: String::new(),
                location: provider.clone(),
                position: 1,
                sandbox_backend: None,
                observed_backend: None,
            },
            crate::models::WorktreeRow {
                worktree: "/blank".into(),
                branch: String::new(),
                agent: String::new(),
                created_at: 0,
                repo_root: String::new(),
                tab_name: String::new(),
                session_name: String::new(),
                location: String::new(),
                position: 2,
                sandbox_backend: None,
                observed_backend: None,
            },
            crate::models::WorktreeRow {
                worktree: "/local".into(),
                branch: String::new(),
                agent: String::new(),
                created_at: 0,
                repo_root: String::new(),
                tab_name: String::new(),
                session_name: String::new(),
                location: "local".into(),
                position: 3,
                sandbox_backend: None,
                observed_backend: None,
            },
            crate::models::WorktreeRow {
                worktree: String::new(),
                branch: String::new(),
                agent: String::new(),
                created_at: 0,
                repo_root: String::new(),
                tab_name: String::new(),
                session_name: String::new(),
                location: ssh.clone(),
                position: 4,
                sandbox_backend: None,
                observed_backend: None,
            },
        ];
        let locations = WorktreeLocations::from_rows(&rows);

        let ssh_loc = locations.loc(Path::new("/r/wt"));
        assert!(ssh_loc.is_remote());
        assert_eq!(ssh_loc.path(), GitLoc::from_db("/r/wt", Some(&ssh)).path());
        let provider_loc = locations.loc(Path::new("/provider/wt"));
        assert!(provider_loc.is_remote());
        assert_eq!(
            provider_loc.path(),
            GitLoc::from_db("/provider/wt", Some(&provider)).path()
        );
        for path in ["/blank", "/local", "/unknown"] {
            assert!(matches!(locations.loc(Path::new(path)), GitLoc::Local(_)));
            assert!(matches!(
                GitLoc::from_db(
                    path,
                    match path {
                        "/blank" => Some(""),
                        "/local" => Some("local"),
                        _ => None,
                    }
                ),
                GitLoc::Local(_)
            ));
        }
        assert!(!locations.by_path.contains_key(""));
    }
}
