//! Launching a Lead tab (THE-2171, THE-2187): the sandboxed agent argv.
//!
//! The Lead runs its agent CLI under bwrap with the whole host read-only, the
//! notes dir and the harness state as the only writable paths, and every
//! control-socket location (the daemon socket dir, the state run dir, each
//! control-socket candidate, `$XDG_RUNTIME_DIR`) masked: the daemon's unix
//! socket grants implicit admin to any same-uid peer, so it must be
//! unreachable from the Lead's shell. The Lead reaches thegn only through its
//! scoped loopback token (`lead::mcp`).
//!
//! The sandbox fails closed: no bwrap means no Lead, never a host shell. This
//! runs off the event loop, inside `handlers::materialize`.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use thegn_core::config::Config;
use thegn_core::lead_sandbox::{self, LeadMountPlan};

/// The refusal when bwrap is missing. Fixed text: the docs and tests cite it.
const NO_BWRAP: &str = "the Lead needs bwrap (Linux); not available here";

/// Host sockets outside `$XDG_RUNTIME_DIR` that grant host command authority
/// (a rootful docker/podman socket is root on the host for a member of its
/// group). Masked with `/dev/null` when present.
const HOST_SOCKETS: &[&str] = &[
    "/run/docker.sock",
    "/var/run/docker.sock",
    "/run/podman/podman.sock",
];

/// The impure inputs of a Lead launch, injected so tests stay hermetic and can
/// force the fail-closed branches.
struct LeadHost<'a> {
    /// Whether bwrap can run here.
    bwrap_available: &'a dyn Fn() -> bool,
    /// Every registered repo root and local worktree path.
    repo_roots: &'a dyn Fn() -> anyhow::Result<Vec<PathBuf>>,
    /// The control-socket directories to hide behind a tmpfs.
    control_dirs: &'a dyn Fn(&Config) -> Vec<PathBuf>,
    /// The MCP plan for this tab (`lead::mcp::prepare`).
    mcp: &'a dyn Fn(&Config, &Path, &str) -> anyhow::Result<crate::lead::mcp::LeadMcpPlan>,
    /// Whether this pane is daemon-owned (drops `--die-with-parent`).
    daemon_owned: bool,
    /// `$XDG_STATE_HOME`: the parent of the Lead's private harness home.
    state_home: PathBuf,
}

/// What a Lead launch is for: a tab's interactive TUI, or one bounded
/// headless turn (`lead.wake` with no tab open). Both share every mount,
/// mask and the fail-closed bwrap; only the command, the caller id and the
/// process wrapper differ.
pub(crate) enum LeadMode<'a> {
    Interactive { group: &'a str, tab: usize },
    Headless { prompt: &'a str },
}

/// The caller id (and token label) of a headless Lead turn. The wake path
/// revokes this label's token once the turn ends.
pub(crate) const HEADLESS_CALLER: &str = "lead:wake/0";

/// A built Lead launch. A headless launch may own a prompt file under the
/// notes dir, removed when this is dropped (after the run is reaped).
pub(crate) struct LeadLaunch {
    pub spec: crate::agent::LaunchSpec,
    #[allow(
        dead_code,
        reason = "held for its drop: removes the prompt file after the reap"
    )]
    prompt_file: Option<WakePromptFile>,
}

impl LeadLaunch {
    /// The prompt file the command reads, when the template uses
    /// `{prompt_file}`.
    #[allow(dead_code, reason = "read by tests; held for its drop")]
    pub(crate) fn prompt_file(&self) -> Option<&Path> {
        self.prompt_file.as_ref().map(|f| f.path.as_path())
    }
}

/// The launch spec for Lead tab `tab` of `group`.
pub(crate) fn launch_spec(
    cfg: &Config,
    group: &str,
    tab: usize,
) -> anyhow::Result<crate::agent::LaunchSpec> {
    launch_spec_with(cfg, LeadMode::Interactive { group, tab }, &real_host(cfg))
        .map(|launch| launch.spec)
}

/// The launch of one headless Lead turn carrying `prompt` (the `lead.wake`
/// envelope): the harness's headless template, rendered with the same
/// quoting and prompt-file transport `agent_run` uses (the prompt file lives
/// under the notes dir), under the very same bwrap plan as a tab. Keep the
/// returned value alive until the process is reaped: it owns the prompt file.
pub(crate) fn headless_spec(cfg: &Config, prompt: &str) -> anyhow::Result<LeadLaunch> {
    launch_spec_with(cfg, LeadMode::Headless { prompt }, &real_host(cfg))
}

fn real_host(cfg: &Config) -> LeadHost<'static> {
    LeadHost {
        bwrap_available: &lead_sandbox::bwrap_available,
        repo_roots: &registered_repo_roots,
        control_dirs: &control_dirs,
        mcp: &crate::lead::mcp::prepare,
        daemon_owned: crate::handlers::startup::daemon_active(cfg),
        state_home: thegn_core::util::xdg_state_home(),
    }
}

fn launch_spec_with(
    cfg: &Config,
    mode: LeadMode<'_>,
    host: &LeadHost<'_>,
) -> anyhow::Result<LeadLaunch> {
    let Some(lead) = cfg.lead.as_ref() else {
        bail!("the Lead is not configured: add a [lead] section naming an [[agents]] entry");
    };
    // Fail closed before touching the disk: no bwrap, no Lead, no host shell.
    if !(host.bwrap_available)() {
        bail!(NO_BWRAP);
    }
    if lead
        .account
        .as_deref()
        .is_some_and(|a| !a.trim().is_empty())
    {
        bail!(
            "[lead] account is not supported yet: the Lead keeps its own private credential \
             home (log in once inside the Lead); remove [lead] account"
        );
    }

    // The notes dir is the only writable dir. Vet it against the repos BEFORE
    // creating it, through its canonical prospective path, so a notes_dir
    // pointed into a repo (directly or through a symlinked ancestor) never
    // creates anything there.
    let repo_roots = canonical_roots((host.repo_roots)().context(
        "the Lead could not read the registered repos to vet its sandbox; refusing to launch",
    )?);
    // The thegn config dir is guarded like a repo: a writable config.toml
    // (agents, gate_command) is host code execution.
    let mut guarded = repo_roots.clone();
    if let Some(dir) = Config::path().parent() {
        guarded.extend(canonical_roots(vec![dir.to_path_buf()]));
    }
    let notes = lead.notes_dir_resolved();
    let prospective = canonical_prospective(&notes);
    lead_sandbox::validate(
        &LeadMountPlan {
            writable: vec![prospective.clone()],
            chdir: prospective,
            ..LeadMountPlan::default()
        },
        &guarded,
    )
    .map_err(|why| anyhow::anyhow!("[lead] notes_dir: {why}"))?;
    if let Some(parent) = notes.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating the Lead notes dir parent {}", parent.display()))?;
    }
    thegn_core::fsperm::ensure_private_dir(&notes)
        .with_context(|| format!("creating the Lead notes dir {}", notes.display()))?;
    let notes = std::fs::canonicalize(&notes)
        .with_context(|| format!("resolving the Lead notes dir {}", notes.display()))?;

    crate::lead::skill::seed(cfg, &notes)?;
    let caller = match &mode {
        LeadMode::Interactive { group, tab } => format!("lead:{group}/{tab}"),
        LeadMode::Headless { .. } => HEADLESS_CALLER.to_string(),
    };

    // The agent: its command (+ harness model flag), its secret-expanded env
    // overlay, then the MCP env, then the Lead's private harness home.
    let eff = thegn_core::agent_task::effective_agent(cfg, &lead.agent, None)
        .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
    let (command, prompt_file) = match &mode {
        LeadMode::Interactive { .. } => (
            eff.interactive_command()
                .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?,
            None,
        ),
        LeadMode::Headless { prompt } => {
            let template = eff
                .headless_template()
                .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
            render_headless(&template, prompt, &notes)?
        }
    };
    // Mint last among the fallible steps that need no mount plan, so a
    // refused render never leaves a live token behind.
    let mcp = (host.mcp)(cfg, &notes, &caller)?;
    let mut env: Vec<(String, String)> = eff.expanded_env();
    env.extend(mcp.env.iter().cloned());
    env.push(("THEGN_CALLER".to_string(), caller));

    let mut cmd = command;
    for arg in &mcp.agent_args {
        cmd.push(' ');
        cmd.push_str(&thegn_core::util::sh_quote(arg));
    }
    let inner = thegn_core::shellinv::exec_argv(&thegn_core::util::shell(), &cmd);

    // The mount plan.
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("the Lead needs $HOME to locate the credentials it hides")?;
    // The harness home is private to the Lead, never the shared `~/.claude`
    // (a hook or MCP entry written there would run unsandboxed in the next
    // worker session). Pushed after the agent env so it overrides any
    // agent-env `CLAUDE_CONFIG_DIR`.
    // A harness with no credential home spec (pi) still relocates its state
    // through `lead_sandbox::private_home_env`; aider writes into its cwd,
    // the notes dir.
    let mut writable = vec![notes.clone()];
    if let Some(h) = thegn_core::harness::harness(&eff.harness)
        && let Some(home_env) = h
            .home()
            .map(|s| s.home_env)
            .filter(|e| !e.is_empty())
            .or_else(|| lead_sandbox::private_home_env(h.id()))
    {
        let rel = Path::new("thegn/lead-harness").join(h.id());
        let dir = thegn_core::fsperm::ensure_private_dir_beneath(&host.state_home, &rel)
            .context("creating the Lead's private harness home")?;
        let dir = std::fs::canonicalize(dir.path())
            .context("resolving the Lead's private harness home")?;
        env.push((home_env.to_string(), dir.to_string_lossy().into_owned()));
        writable.push(dir);
    }
    writable.dedup();
    // Parents first, so a dir already covered by a mask is neither created
    // nor masked twice.
    let mut control = (host.control_dirs)(cfg);
    control.sort();
    control.dedup();
    let mut masked: Vec<PathBuf> = Vec::new();
    for dir in control {
        if masked.iter().any(|m| dir.starts_with(m)) {
            continue;
        }
        if let Some(dir) = prepare_mask_dir(&dir)? {
            masked.push(dir);
        }
    }
    let mut masked_files = HOST_SOCKETS
        .iter()
        .map(PathBuf::from)
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| !m.is_dir()))
        .collect::<Vec<_>>();
    // Key files and forge tokens: the Lead never pushes, and the read-only
    // root would otherwise expose them.
    const HOME_CREDENTIALS: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".config/gh",
        ".config/glab-cli",
        ".git-credentials",
        ".netrc",
    ];
    for c in HOME_CREDENTIALS {
        let path = home.join(c);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                let dir = std::fs::canonicalize(&path).unwrap_or(path);
                if !masked.iter().any(|m| dir.starts_with(m)) {
                    masked.push(dir);
                }
            }
            Ok(_) => masked_files.push(path),
            Err(_) => {}
        }
    }
    let readable = readable_under_masks(&mcp.readable, &masked)?;
    let plan = LeadMountPlan {
        writable,
        masked,
        masked_files,
        readable,
        chdir: notes.clone(),
        // A headless turn is a plain child of the blocking thread that waits
        // on it for the whole run: it always dies with its parent.
        die_with_parent: matches!(mode, LeadMode::Headless { .. }) || !host.daemon_owned,
    };
    lead_sandbox::validate(&plan, &guarded)
        .map_err(|why| anyhow::anyhow!("the Lead sandbox was refused: {why}"))?;

    let sandboxed = lead_sandbox::bwrap_argv(&plan, &inner);
    let argv = match mode {
        LeadMode::Interactive { .. } => {
            thegn_core::sandbox_cpucap::wrap_uncontained_pane_argv(sandboxed)
        }
        LeadMode::Headless { .. } => thegn_core::sandbox_cpucap::wrap_background_argv_tagged(
            sandboxed,
            &thegn_core::sandbox_cpucap::ScopeTag {
                kind: thegn_core::sandbox_cpucap::ScopeKind::Agent,
                worktree: notes.to_string_lossy().into_owned(),
            },
        ),
    };
    Ok(LeadLaunch {
        spec: crate::agent::LaunchSpec {
            argv,
            cwd: Some(notes),
            env,
            backend: "bwrap".to_string(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        },
        prompt_file,
    })
}

/// The notes-dir subdirectory holding headless prompt files.
const WAKE_PROMPT_DIR: &str = ".wake";

/// Render a headless template with `prompt` exactly as `agent_run` does:
/// `{prompt}` is shell-quoted by `substitute_command`; a `{prompt_file}`
/// template gets an owner-only file under the notes dir (inside the sandbox
/// at the same path) instead. Bounded like a queued agent's command.
fn render_headless(
    template: &str,
    prompt: &str,
    notes: &Path,
) -> anyhow::Result<(String, Option<WakePromptFile>)> {
    crate::agent_run::check_agent_payload(prompt).map_err(anyhow::Error::msg)?;
    let (vars, file) = if crate::agent_run::uses_prompt_file(template) {
        let dir = thegn_core::fsperm::ensure_private_dir_beneath(notes, Path::new(WAKE_PROMPT_DIR))
            .context("creating the Lead's prompt-file dir")?;
        let file = WakePromptFile::write(dir, prompt)?;
        let path = file
            .path
            .to_str()
            .context("{prompt_file}: the prompt file path is not valid UTF-8")?
            .to_string();
        (
            thegn_core::agent_task::TaskVars::new().set("prompt_file", path),
            Some(file),
        )
    } else {
        (thegn_core::agent_task::TaskVars::new(), None)
    };
    let command = thegn_core::agent_task::substitute_command(template, prompt, &vars)
        .map_err(|e| anyhow::anyhow!("[lead] agent: headless template: {e}"))?;
    if command.len() > crate::agent_run::MAX_RENDERED_COMMAND_BYTES {
        bail!("[lead] agent: the headless Lead command exceeds the safe argument limit");
    }
    Ok((command, file))
}

/// An owner-only prompt file under the notes dir, removed on drop.
struct WakePromptFile {
    dir: thegn_core::fsperm::PrivateDir,
    name: String,
    path: PathBuf,
}

impl WakePromptFile {
    fn write(dir: thegn_core::fsperm::PrivateDir, prompt: &str) -> anyhow::Result<Self> {
        use std::io::Write as _;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("prompt-{}-{nanos}-{seq}", std::process::id());
        let mut file = dir
            .create_owner_only_exclusive(&name)
            .context("creating the Lead's prompt file")?;
        let path = dir.path().join(&name);
        let guard = Self { dir, name, path };
        file.write_all(prompt.as_bytes())
            .and_then(|()| file.flush())
            .context("writing the Lead's prompt file")?;
        Ok(guard)
    }
}

impl Drop for WakePromptFile {
    fn drop(&mut self) {
        if let Err(error) = self.dir.remove_file(&self.name) {
            tracing::debug!(%error, "unable to remove the Lead's prompt file");
        }
    }
}

/// A control dir to mask. Under `/tmp` it is already covered by the private
/// tmpfs. A missing one is created owner-only so a socket the daemon binds
/// there later is still hidden (a tmpfs needs an existing mount point under
/// the read-only root). A non-directory cannot take a tmpfs: refused.
fn prepare_mask_dir(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    if !dir.is_absolute() || dir.starts_with("/tmp") {
        return Ok(None);
    }
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!(
            "the Lead sandbox cannot mask {}: not a directory; refusing to launch",
            dir.display()
        ),
        Err(_) => thegn_core::fsperm::ensure_private_dir(dir).with_context(|| {
            format!(
                "creating the control dir {} to mask it from the Lead",
                dir.display()
            )
        })?,
    }
    Ok(Some(
        std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
    ))
}

/// The MCP plan's readable paths that a mask would hide: re-bound read-only.
/// Only regular files qualify; re-exposing anything else under a mask (a
/// socket, a directory of sockets) would undo the mask, so it is refused.
fn readable_under_masks(readable: &[PathBuf], masked: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in readable {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if !masked.iter().any(|m| path.starts_with(m)) {
            continue;
        }
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            bail!(
                "the Lead sandbox cannot re-expose {} under a masked control dir: only regular \
                 files may be; refusing to launch",
                path.display()
            );
        }
        out.push(path);
    }
    Ok(out)
}

/// Every control-socket location: the daemon socket dir, the state run dir,
/// the parent of every control-socket candidate, and `$XDG_RUNTIME_DIR` as a
/// whole (it also holds the user bus and the podman/systemd sockets).
fn control_dirs(cfg: &Config) -> Vec<PathBuf> {
    let state_dir = thegn_core::util::xdg_state_home().join("thegn");
    let mut dirs = Vec::new();
    if let Some(dir) =
        crate::daemon::control_publish::socket_dir(&crate::daemon::socket_path(&cfg.daemon))
    {
        dirs.push(dir);
    }
    dirs.push(crate::daemon::control_publish::run_dir(&state_dir));
    for socket in crate::daemon::control_publish::control_socket_candidates(&cfg.daemon) {
        if let Some(dir) = crate::daemon::control_publish::socket_dir(&socket) {
            dirs.push(dir);
        }
    }
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|r| !r.is_empty()) {
        dirs.push(PathBuf::from(run));
    }
    dirs
}

/// Every registered repo root and local worktree, from the state DB.
fn registered_repo_roots() -> anyhow::Result<Vec<PathBuf>> {
    use thegn_core::store::WorkspaceStore;
    let db = thegn_core::db::Db::open()?;
    let mut roots: Vec<PathBuf> = db
        .workspaces()?
        .into_iter()
        .map(|w| PathBuf::from(w.repo_path))
        .collect();
    roots.extend(db.known_repos()?.into_iter().map(PathBuf::from));
    roots.extend(
        db.worktrees()?
            .into_iter()
            .filter(|w| w.location.trim().is_empty())
            .flat_map(|w| [PathBuf::from(w.worktree), PathBuf::from(w.repo_root)]),
    );
    Ok(roots)
}

/// Roots as given plus their canonical forms (a symlinked repo path must not
/// let a writable bind slip past the comparison). Non-absolute and empty
/// entries are dropped.
fn canonical_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        if !root.is_absolute() {
            continue;
        }
        if let Ok(canonical) = std::fs::canonicalize(&root) {
            out.push(canonical);
        }
        out.push(root);
    }
    out.sort();
    out.dedup();
    out
}

/// Where `path` will resolve once created: its nearest existing ancestor,
/// canonicalized, joined with the missing tail.
fn canonical_prospective(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cur) {
            let mut out = canonical;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        cfg: Config,
    }

    fn fixture() -> Fixture {
        // outside /tmp: the Lead's private /tmp swallows masks there
        let tmp = tempfile::Builder::new()
            .prefix(".lead-launch-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let cfg = Config {
            agents: vec![toml::from_str("name = \"lead-agent\"\ncommand = \"true\"").unwrap()],
            lead: Some(thegn_core::config_lead::LeadConfig {
                agent: "lead-agent".into(),
                notes_dir: Some(root.join("notes")),
                ..Default::default()
            }),
            ..Config::default()
        };
        Fixture {
            _tmp: tmp,
            root,
            cfg,
        }
    }

    fn no_mcp(_: &Config, _: &Path, _: &str) -> anyhow::Result<crate::lead::mcp::LeadMcpPlan> {
        Ok(crate::lead::mcp::LeadMcpPlan::default())
    }

    fn run(
        fx: &Fixture,
        bwrap: bool,
        repos: Vec<PathBuf>,
        control: Vec<PathBuf>,
    ) -> anyhow::Result<crate::agent::LaunchSpec> {
        run_mode(
            fx,
            LeadMode::Interactive {
                group: "lead",
                tab: 0,
            },
            bwrap,
            repos,
            control,
        )
        .map(|launch| launch.spec)
    }

    fn run_mode(
        fx: &Fixture,
        mode: LeadMode<'_>,
        bwrap: bool,
        repos: Vec<PathBuf>,
        control: Vec<PathBuf>,
    ) -> anyhow::Result<LeadLaunch> {
        let available = move || bwrap;
        let roots = move || -> anyhow::Result<Vec<PathBuf>> { Ok(repos.clone()) };
        let dirs = move |_: &Config| control.clone();
        launch_spec_with(
            &fx.cfg,
            mode,
            &LeadHost {
                bwrap_available: &available,
                repo_roots: &roots,
                control_dirs: &dirs,
                mcp: &no_mcp,
                daemon_owned: true,
                state_home: fx.root.join("state"),
            },
        )
    }

    fn headless(fx: &Fixture, prompt: &str, bwrap: bool) -> anyhow::Result<LeadLaunch> {
        run_mode(fx, LeadMode::Headless { prompt }, bwrap, vec![], vec![])
    }

    fn set_agent(fx: &mut Fixture, toml_entry: &str) {
        fx.cfg.agents = vec![toml::from_str(toml_entry).unwrap()];
    }

    /// The shell command bwrap runs (the last argv element).
    fn inner_command(spec: &crate::agent::LaunchSpec) -> String {
        spec.argv.last().cloned().unwrap_or_default()
    }

    #[test]
    fn headless_spec_without_bwrap_bails_and_creates_nothing() {
        let fx = fixture();
        let err = headless(&fx, "hi", false).err().expect("refused");
        assert_eq!(err.to_string(), NO_BWRAP);
        assert!(!fx.root.join("notes").exists());
    }

    #[test]
    fn headless_spec_without_lead_config_bails() {
        let mut fx = fixture();
        fx.cfg.lead = None;
        let err = headless(&fx, "hi", true).err().expect("refused");
        assert!(err.to_string().contains("[lead]"), "{err}");
    }

    #[test]
    fn headless_spec_wraps_the_same_bwrap_plan_as_a_tab() {
        let fx = fixture();
        let sock_dir = fx.root.join("run-sock");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let tab = run(&fx, true, vec![], vec![sock_dir.clone()]).unwrap();
        let wake = run_mode(
            &fx,
            LeadMode::Headless { prompt: "go" },
            true,
            vec![],
            vec![sock_dir.clone()],
        )
        .unwrap()
        .spec;
        let notes_s = fx.root.join("notes").display().to_string();
        let sock_s = sock_dir.display().to_string();
        assert_eq!(wake.backend, "bwrap");
        assert!(has(&wake.argv, &["--ro-bind", "/", "/"]), "{:?}", wake.argv);
        assert!(has(&wake.argv, &["--bind", &notes_s, &notes_s]));
        assert!(has(&wake.argv, &["--tmpfs", &sock_s]));
        assert!(has(&wake.argv, &["--chdir", &notes_s]));
        assert!(
            wake.argv.iter().any(|a| a == "--die-with-parent"),
            "a headless turn dies with its waiting thread"
        );
        // Every mount, mask and unset of the tab is present in the wake argv.
        let mounts = |argv: &[String]| -> Vec<String> {
            let b = argv.iter().position(|a| a == "bwrap").unwrap();
            let sep = argv.iter().position(|a| a == "--").unwrap();
            argv[b + 1..sep]
                .iter()
                .filter(|a| *a != "--die-with-parent")
                .cloned()
                .collect()
        };
        assert_eq!(mounts(&tab.argv), mounts(&wake.argv));
        assert!(
            wake.env
                .iter()
                .any(|(k, v)| k == "THEGN_CALLER" && v == HEADLESS_CALLER)
        );
    }

    #[test]
    fn headless_spec_never_puts_the_prompt_in_argv_unquoted() {
        let fx = fixture();
        for prompt in [
            "hi $(touch pwned) `id` ; rm -rf ~",
            "it's '; touch pwned; echo '",
            "line1\nline2 && reboot",
        ] {
            let launch = headless(&fx, prompt, true).unwrap();
            let cmd = inner_command(&launch.spec);
            let quoted = thegn_core::util::sh_quote(prompt);
            assert!(cmd.contains(&quoted), "{cmd}");
            // The only occurrence of the raw text is inside its quoting.
            let without = cmd.replacen(&quoted, "", 1);
            assert!(!without.contains("touch pwned"), "{cmd}");
            assert!(!without.contains("reboot"), "{cmd}");
            assert!(
                launch
                    .spec
                    .argv
                    .iter()
                    .all(|a| !a.contains("pwned") || *a == cmd),
                "the prompt reaches only the single inner shell command"
            );
        }
    }

    #[test]
    fn headless_spec_carries_a_prompt_file_under_the_notes_dir() {
        let mut fx = fixture();
        set_agent(
            &mut fx,
            "name = \"lead-agent\"\ncommand = \"true {prompt_file}\"",
        );
        let launch = headless(&fx, "secret prompt", true).unwrap();
        let file = launch.prompt_file().expect("prompt file").to_path_buf();
        assert!(
            file.starts_with(fx.root.join("notes")),
            "{}",
            file.display()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "secret prompt");
        let cmd = inner_command(&launch.spec);
        assert!(!cmd.contains("secret prompt"), "{cmd}");
        assert!(cmd.contains(&thegn_core::util::sh_quote(&file.to_string_lossy())));
        drop(launch);
        assert!(!file.exists(), "the prompt file is removed with the launch");
    }

    #[test]
    fn headless_spec_gives_pi_a_private_writable_home() {
        let mut fx = fixture();
        set_agent(&mut fx, "name = \"lead-agent\"\ncommand = \"pi\"");
        let launch = headless(&fx, "hi", true).unwrap();
        let home = fx.root.join("state/thegn/lead-harness/pi");
        let home_s = home.display().to_string();
        assert!(home.is_dir());
        assert!(
            launch
                .spec
                .env
                .iter()
                .any(|(k, v)| k == "PI_CODING_AGENT_DIR" && *v == home_s),
            "{:?}",
            launch.spec.env
        );
        assert!(has(&launch.spec.argv, &["--bind", &home_s, &home_s]));
        // The interactive tab gets the same private home.
        let tab = run(&fx, true, vec![], vec![]).unwrap();
        assert!(has(&tab.argv, &["--bind", &home_s, &home_s]));
    }

    fn has(argv: &[String], needle: &[&str]) -> bool {
        argv.windows(needle.len())
            .any(|w| w.iter().map(String::as_str).eq(needle.iter().copied()))
    }

    #[test]
    fn launch_spec_without_lead_config_bails() {
        let mut fx = fixture();
        fx.cfg.lead = None;
        let err = run(&fx, true, vec![], vec![]).unwrap_err();
        assert!(err.to_string().contains("[lead]"), "{err}");
    }

    #[test]
    fn launch_spec_without_bwrap_bails_and_never_returns_a_host_argv() {
        let fx = fixture();
        let err = run(&fx, false, vec![], vec![]).unwrap_err();
        assert_eq!(err.to_string(), NO_BWRAP);
        assert!(
            !fx.root.join("notes").exists(),
            "a refused launch creates nothing"
        );
    }

    #[test]
    fn launch_spec_wraps_the_agent_in_a_read_only_bwrap() {
        let fx = fixture();
        let sock_dir = fx.root.join("run-sock");
        let state_run = fx.root.join("state-run");
        std::fs::create_dir_all(&sock_dir).unwrap();
        let spec = run(
            &fx,
            true,
            vec![fx.root.join("repo")],
            vec![sock_dir.clone(), state_run.clone()],
        )
        .unwrap();
        let notes = fx.root.join("notes");
        let notes_s = notes.display().to_string();
        let sock_s = sock_dir.display().to_string();
        let run_s = state_run.display().to_string();
        assert_eq!(spec.backend, "bwrap");
        assert_eq!(spec.cwd.as_deref(), Some(notes.as_path()));
        assert!(!spec.remote && !spec.degraded);
        assert!(has(&spec.argv, &["--ro-bind", "/", "/"]), "{:?}", spec.argv);
        assert!(has(&spec.argv, &["--bind", &notes_s, &notes_s]));
        assert!(has(&spec.argv, &["--tmpfs", &sock_s]), "{:?}", spec.argv);
        assert!(
            state_run.is_dir(),
            "a missing control dir is created to be masked"
        );
        assert!(has(&spec.argv, &["--tmpfs", &run_s]));
        assert!(has(&spec.argv, &["--chdir", &notes_s]));
        assert!(
            !spec.argv.iter().any(|a| a == "--die-with-parent"),
            "a daemon-owned pane drops --die-with-parent like the terminal path"
        );
        let bwrap = spec.argv.iter().position(|a| a == "bwrap").expect("bwrap");
        let sep = spec.argv.iter().position(|a| a == "--").unwrap();
        assert!(bwrap < sep && spec.argv[sep + 1..].iter().any(|a| a.contains("true")));
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "THEGN_CALLER" && v == "lead:lead/0")
        );
    }

    #[test]
    fn launch_spec_refuses_a_notes_dir_inside_a_repo_without_creating_it() {
        let mut fx = fixture();
        let repo = fx.root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if let Some(lead) = fx.cfg.lead.as_mut() {
            lead.notes_dir = Some(repo.join("notes"));
        }
        let err = run(&fx, true, vec![repo.clone()], vec![]).unwrap_err();
        assert!(err.to_string().contains("notes_dir"), "{err}");
        assert!(!repo.join("notes").exists());
    }

    #[test]
    fn launch_spec_refuses_a_notes_dir_symlinked_into_a_repo() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let mut fx = fixture();
        let repo = fx.root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let link = fx.root.join("link");
        crate::platform::test_symlink(&repo, &link).unwrap();
        if let Some(lead) = fx.cfg.lead.as_mut() {
            lead.notes_dir = Some(link.join("notes"));
        }
        assert!(run(&fx, true, vec![repo.clone()], vec![]).is_err());
        assert!(!repo.join("notes").exists());
    }

    #[test]
    fn launch_spec_refuses_a_notes_dir_inside_a_masked_control_dir() {
        let mut fx = fixture();
        let control = fx.root.join("control");
        std::fs::create_dir_all(&control).unwrap();
        if let Some(lead) = fx.cfg.lead.as_mut() {
            lead.notes_dir = Some(control.join("notes"));
        }
        assert!(run(&fx, true, vec![], vec![control]).is_err());
    }

    #[test]
    fn readable_under_a_mask_must_be_a_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let mask = root.join("run");
        std::fs::create_dir_all(mask.join("sub")).unwrap();
        std::fs::write(mask.join("token"), "t").unwrap();
        let outside = root.join("elsewhere");
        let got = readable_under_masks(&[mask.join("token"), outside], std::slice::from_ref(&mask))
            .unwrap();
        assert_eq!(got, vec![mask.join("token")]);
        assert!(readable_under_masks(&[mask.join("sub")], &[mask]).is_err());
    }

    #[test]
    fn prepare_mask_dir_skips_tmp_and_refuses_a_file() {
        assert_eq!(prepare_mask_dir(Path::new("/tmp/thegn-x")).unwrap(), None);
        assert_eq!(prepare_mask_dir(Path::new("rel")).unwrap(), None);
        // outside /tmp: the Lead's private /tmp swallows masks there
        let tmp = tempfile::Builder::new()
            .prefix(".lead-launch-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, "").unwrap();
        assert!(prepare_mask_dir(&file).is_err());
    }

    #[test]
    fn canonical_prospective_resolves_the_existing_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(
            canonical_prospective(&root.join("a/b")),
            root.join("a").join("b")
        );
    }
}
