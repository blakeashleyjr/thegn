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

/// The launch spec for Lead tab `tab` of `group`.
pub(crate) fn launch_spec(
    cfg: &Config,
    group: &str,
    tab: usize,
) -> anyhow::Result<crate::agent::LaunchSpec> {
    launch_spec_with(
        cfg,
        group,
        tab,
        &LeadHost {
            bwrap_available: &lead_sandbox::bwrap_available,
            repo_roots: &registered_repo_roots,
            control_dirs: &control_dirs,
            mcp: &crate::lead::mcp::prepare,
            daemon_owned: crate::handlers::startup::daemon_active(cfg),
            state_home: thegn_core::util::xdg_state_home(),
        },
    )
}

fn launch_spec_with(
    cfg: &Config,
    group: &str,
    tab: usize,
    host: &LeadHost<'_>,
) -> anyhow::Result<crate::agent::LaunchSpec> {
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
    let caller = format!("lead:{group}/{tab}");
    let mcp = (host.mcp)(cfg, &notes, &caller)?;

    // The agent: its command (+ harness model flag), its secret-expanded env
    // overlay, then the MCP env, then the Lead's private harness home.
    let eff = thegn_core::agent_task::effective_agent(cfg, &lead.agent, None)
        .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
    let command = eff
        .interactive_command()
        .map_err(|e| anyhow::anyhow!("[lead] agent: {e}"))?;
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
    let mut writable = vec![notes.clone()];
    if let Some(h) = thegn_core::harness::harness(&eff.harness)
        && let Some(spec) = h.home().filter(|s| !s.home_env.is_empty())
    {
        let rel = Path::new("thegn/lead-harness").join(h.id());
        let dir = thegn_core::fsperm::ensure_private_dir_beneath(&host.state_home, &rel)
            .context("creating the Lead's private harness home")?;
        let dir = std::fs::canonicalize(dir.path())
            .context("resolving the Lead's private harness home")?;
        env.push((
            spec.home_env.to_string(),
            dir.to_string_lossy().into_owned(),
        ));
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
        die_with_parent: !host.daemon_owned,
    };
    lead_sandbox::validate(&plan, &guarded)
        .map_err(|why| anyhow::anyhow!("the Lead sandbox was refused: {why}"))?;

    let argv = thegn_core::sandbox_cpucap::wrap_uncontained_pane_argv(lead_sandbox::bwrap_argv(
        &plan, &inner,
    ));
    Ok(crate::agent::LaunchSpec {
        argv,
        cwd: Some(notes),
        env,
        backend: "bwrap".to_string(),
        warnings: Vec::new(),
        degraded: false,
        remote: false,
    })
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
        let available = move || bwrap;
        let roots = move || -> anyhow::Result<Vec<PathBuf>> { Ok(repos.clone()) };
        let dirs = move |_: &Config| control.clone();
        launch_spec_with(
            &fx.cfg,
            "lead",
            0,
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
