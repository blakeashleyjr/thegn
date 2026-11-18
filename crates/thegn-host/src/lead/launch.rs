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
//! b155: thegn's state/data/cache dirs and every harness home are masked as
//! well (THE-2221; only the notes dir, the private harness home and the MCP
//! files are re-exposed), and with `[lead] network = "isolated"` (the
//! default) the bwrap argv runs inside `pasta`'s own network namespace with
//! only the Lead listener port forwarded (THE-2220).
//!
//! The sandbox fails closed: no bwrap means no Lead, never a host shell; an
//! isolated Lead without pasta refuses too, never falling back to the shared
//! network. This runs off the event loop, inside `handlers::materialize`.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use thegn_core::config::Config;
use thegn_core::config_lead::LeadNetwork;
use thegn_core::lead_sandbox::{self, LeadMountPlan};

/// The refusal when bwrap is missing. Fixed text: the docs and tests cite it.
const NO_BWRAP: &str = "the Lead needs bwrap (Linux); not available here";
/// The refusal when the isolated network has no pasta. Fixed text: the docs
/// and tests cite it.
const NO_PASTA: &str = "the Lead's isolated network needs pasta (passt); install it or set \
                        [lead] network = \"host\"";
/// Where the isolated network's generated `resolv.conf` lives, relative to
/// `$XDG_STATE_HOME`.
const NET_DIR_REL: &str = "thegn/lead-net";
const RESOLV_CONF_FILE: &str = "resolv.conf";

/// Host sockets outside `$XDG_RUNTIME_DIR` that grant host command authority
/// (a rootful docker/podman socket is root on the host for a member of its
/// group). Masked with `/dev/null` when present.
const HOST_SOCKETS: &[&str] = &[
    "/run/docker.sock",
    "/var/run/docker.sock",
    "/run/podman/podman.sock",
    // The system bus: a unix path socket is not scoped to a network
    // namespace, and polkit `allow_active` actions (udisks mount, logind
    // power, NetworkManager) are reachable through it.
    "/run/dbus/system_bus_socket",
    "/var/run/dbus/system_bus_socket",
];

/// The impure inputs of a Lead launch, injected so tests stay hermetic and can
/// force the fail-closed branches.
struct LeadHost<'a> {
    /// Whether bwrap can run here.
    bwrap_available: &'a dyn Fn() -> bool,
    /// Whether pasta (passt) is installed, for `[lead] network = "isolated"`.
    pasta_available: &'a dyn Fn() -> bool,
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
    /// `$XDG_DATA_HOME` and `$XDG_CACHE_HOME`: their `thegn` dirs are masked.
    data_home: PathBuf,
    cache_home: PathBuf,
    /// `$HOME` (credentials and harness homes are masked beneath it).
    home: Option<PathBuf>,
    /// thegn's own home (`util::thegn_dir()`): masked, with registered repos
    /// beneath it re-exposed read-only.
    thegn_dir: PathBuf,
    /// The config file (`Config::path()`): its dir is masked and only the
    /// file itself is re-exposed read-only.
    config_path: PathBuf,
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
        pasta_available: &thegn_core::lead_net::pasta_available,
        repo_roots: &registered_repo_roots,
        control_dirs: &control_dirs,
        mcp: &crate::lead::mcp::prepare,
        daemon_owned: crate::handlers::startup::daemon_active(cfg),
        state_home: thegn_core::util::xdg_state_home(),
        data_home: xdg_home("XDG_DATA_HOME", ".local/share"),
        cache_home: xdg_home("XDG_CACHE_HOME", ".cache"),
        home: env_home(),
        thegn_dir: thegn_core::util::thegn_dir(),
        config_path: Config::path(),
    }
}

fn env_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `$<var>` when absolute, else `$HOME/<fallback>` (the XDG rule).
fn xdg_home(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env_home().map(|h| h.join(fallback)))
        .unwrap_or_else(|| PathBuf::from("/nonexistent").join(fallback))
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
    // Same for the isolated network: no pasta, no Lead. Never the shared
    // network unless `[lead] network = "host"` says so.
    let network = lead.network;
    if network == LeadNetwork::Isolated && !(host.pasta_available)() {
        bail!(NO_PASTA);
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
        LeadMode::Interactive { group, tab } => crate::lead::mcp::tab_caller(group, *tab),
        LeadMode::Headless { .. } => HEADLESS_CALLER.to_string(),
    };
    // Only this Lead's own MCP file is re-exposed from the control dir, never
    // another tab's (or the wake turn's) token.
    let own_mcp = crate::lead::mcp::mcp_file_name(&caller);

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
    // The isolated network's resolv.conf, written before the mint so a
    // failed write never leaves a live token behind.
    let resolv_conf = if network == LeadNetwork::Isolated {
        let dir = thegn_core::fsperm::ensure_private_dir_beneath(
            &host.state_home,
            Path::new(NET_DIR_REL),
        )
        .context("creating the Lead's network dir")?;
        let file = dir
            .write_owner_only_atomic(
                RESOLV_CONF_FILE,
                thegn_core::lead_net::resolv_conf().as_bytes(),
            )
            .context("writing the Lead's resolv.conf")?;
        Some(std::fs::canonicalize(&file).unwrap_or(file))
    } else {
        None
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
    let home = host
        .home
        .clone()
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
        let rel = Path::new("thegn")
            .join(lead_sandbox::HARNESS_HOME_DIR)
            .join(h.id());
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
    // thegn's own state/data/cache dirs, its home, the legacy pre-rename
    // dirs and the shared harness homes (THE-2221): canonical, parents
    // first. thegn's own dirs are created when missing (a dir thegn creates
    // later would otherwise show through the live root until the Lead
    // restarts); the rest are masked only when present (a tmpfs needs a
    // mount point under the read-only root). The notes dir, the private
    // harness home and the MCP files are re-exposed after them (bwrap binds
    // every source from the host root and creates the mount points inside
    // the tmpfs).
    let created_when_missing = [
        host.state_home.join("thegn"),
        host.data_home.join("thegn"),
        host.cache_home.join("thegn"),
        host.thegn_dir.clone(),
    ];
    let mut state_masked: Vec<PathBuf> = Vec::new();
    let mut candidates = lead_sandbox::state_masks(
        &host.state_home,
        &host.data_home,
        &host.cache_home,
        &home,
        &host.thegn_dir,
    );
    candidates.sort();
    candidates.dedup();
    for dir in candidates {
        if !dir.is_absolute() || dir.starts_with("/tmp") {
            continue;
        }
        if created_when_missing.contains(&dir) && std::fs::symlink_metadata(&dir).is_err() {
            thegn_core::fsperm::ensure_private_dir(&dir)
                .with_context(|| format!("creating {} to mask it from the Lead", dir.display()))?;
        }
        if !std::fs::metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        if state_masked.iter().any(|m| dir.starts_with(m)) {
            continue;
        }
        state_masked.push(dir);
    }
    for file in lead_sandbox::state_masked_files(&home) {
        if std::fs::metadata(&file).is_ok_and(|m| !m.is_dir()) {
            masked_files.push(file);
        }
    }
    // The config dir holds signing keys, secret files and backups that may
    // carry old literal tokens: masked, with only `config.toml` re-exposed.
    // `(source, in-sandbox path)`: a config.toml symlinked out of the dir
    // (a store path) is bound back at its place inside the mask.
    let mut config_reexposure: Option<(PathBuf, PathBuf)> = None;
    if let Some(dir) = host.config_path.parent()
        && std::fs::metadata(dir).is_ok_and(|m| m.is_dir())
    {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if !dir.starts_with("/tmp") && !state_masked.iter().any(|m| dir.starts_with(m)) {
            state_masked.push(dir.clone());
        }
        if std::fs::metadata(&host.config_path).is_ok_and(|m| m.is_file())
            && let (Ok(src), Some(name)) = (
                std::fs::canonicalize(&host.config_path),
                host.config_path.file_name(),
            )
        {
            config_reexposure = Some((src, dir.join(name)));
        }
    }
    // Files named by `file:` secret refs read as empty, at both the named
    // path and its resolved target; `env:` refs of the push and inbox
    // secrets are unset (both halves of a phone approval must stay out of
    // the Lead's reach).
    let mut unset_env: Vec<String> = Vec::new();
    for field in thegn_core::secret_scan::secret_refs(cfg) {
        if let thegn_core::secretref::SecretRef::File { path } = &field.reference {
            hide_secret_file(path, &mut masked_files);
        }
    }
    let push = &cfg.notifications.push;
    let push_secrets = std::iter::once(&push.token)
        .chain(std::iter::once(&push.inbox.inbox_secret))
        .chain(push.sinks.iter().map(|sink| &sink.token));
    for raw in push_secrets {
        if raw.trim().is_empty() {
            continue;
        }
        match thegn_core::secretref::SecretRef::parse(raw, thegn_core::secretref::BareAs::Literal) {
            thegn_core::secretref::SecretRef::Env { var } if !var.is_empty() => {
                if !unset_env.contains(&var) {
                    unset_env.push(var);
                }
            }
            thegn_core::secretref::SecretRef::File { path } => {
                hide_secret_file(&path, &mut masked_files);
            }
            _ => {}
        }
    }
    masked_files.sort();
    masked_files.dedup();
    let all_masks: Vec<PathBuf> = masked.iter().chain(&state_masked).cloned().collect();
    let mut readable = readable_under_masks(
        &mcp.readable,
        &all_masks,
        &[own_mcp.as_str(), crate::lead::mcp::CONTROL_ADDR_FILE],
    )?;
    let mut file_binds: Vec<(PathBuf, PathBuf)> = Vec::new();
    match config_reexposure {
        Some((src, dst)) if src == dst => readable.push(src),
        Some(pair) => file_binds.push(pair),
        None => {}
    }
    // `thegn_dir()/worktrees` is the default worktree root: every registered
    // repo beneath a state mask comes back read-only (never writable).
    let mut reexposed_repos: Vec<PathBuf> = repo_roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .filter(|root| {
            state_masked
                .iter()
                .any(|m| root.starts_with(m) && root != m)
        })
        .collect();
    reexposed_repos.sort();
    reexposed_repos.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for root in reexposed_repos {
        if !kept.iter().any(|k| root.starts_with(k)) {
            kept.push(root);
        }
    }
    readable.extend(kept);

    // The network (THE-2220).
    let listener_port = match network {
        LeadNetwork::Isolated => {
            let addr = mcp.control_addr.context(
                "the Lead's isolated network needs the daemon's Lead listener address; \
                 restart the daemon",
            )?;
            if let Some(file) = resolv_conf {
                file_binds.push((file, PathBuf::from("/etc/resolv.conf")));
            }
            Some(addr.port())
        }
        LeadNetwork::Host => {
            tracing::warn!(
                target: "thegn::lead",
                "[lead] network = \"host\": the Lead shares the host network namespace and can \
                 reach every host loopback service and abstract unix socket"
            );
            None
        }
    };

    let plan = LeadMountPlan {
        writable,
        masked,
        state_masked,
        masked_files,
        readable,
        file_binds,
        chdir: notes.clone(),
        // A headless turn is a plain child of the blocking thread that waits
        // on it for the whole run: it always dies with its parent.
        die_with_parent: matches!(mode, LeadMode::Headless { .. }) || !host.daemon_owned,
        // pasta starts bwrap as uid 0 of its user namespace: map back to the
        // caller's ids (claude refuses to skip permissions as root, and host
        // files would read as root-owned). Host mode keeps today's argv.
        sandbox_ids: match network {
            LeadNetwork::Isolated => crate::platform::current_ids(),
            LeadNetwork::Host => None,
        },
        unset_env,
    };
    lead_sandbox::validate(&plan, &guarded)
        .map_err(|why| anyhow::anyhow!("the Lead sandbox was refused: {why}"))?;

    let sandboxed = lead_sandbox::bwrap_argv(&plan, &inner);
    let sandboxed = match listener_port {
        Some(port) => thegn_core::lead_net::pasta_argv(port, &sandboxed),
        None => sandboxed,
    };
    tracing::debug!(
        target: "thegn::lead",
        backend = "bwrap",
        network = network.as_str(),
        "the Lead sandbox is ready"
    );
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
/// socket, a directory of sockets) would undo the mask. A directory (the
/// Lead control dir, now beneath the masked state dir) is expanded to the
/// regular files directly inside it; any other entry stays hidden. Any
/// other non-file path is refused.
/// Only files named in `keep` are taken from an expanded directory (the
/// Lead's own MCP file and the listener address, never another Lead's).
fn readable_under_masks(
    readable: &[PathBuf],
    masked: &[PathBuf],
    keep: &[&str],
) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in readable {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if !masked.iter().any(|m| path.starts_with(m)) {
            continue;
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&path)
                .with_context(|| format!("listing {} to re-expose it", path.display()))?
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter(|e| e.file_name().to_str().is_some_and(|n| keep.contains(&n)))
                .map(|e| e.path())
                .collect();
            files.sort();
            out.extend(files);
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

/// Hide a `file:` secret ref's file (and its resolved target when that
/// differs) behind `/dev/null`. Missing paths and directories are skipped.
fn hide_secret_file(path: &str, masked_files: &mut Vec<PathBuf>) {
    let path = PathBuf::from(thegn_core::util::expand_tilde(path.trim()));
    if !path.is_absolute() || !std::fs::metadata(&path).is_ok_and(|m| !m.is_dir()) {
        return;
    }
    if let Ok(target) = std::fs::canonicalize(&path)
        && target != path
    {
        masked_files.push(target);
    }
    masked_files.push(path);
}

/// The roots the Lead sandbox masks, for `thegn doctor` and `thegn lead
/// login`. Stub created by the b161 spine; b161 lane 3 fills it.
#[allow(dead_code, reason = "filled by b161 lane 3")]
pub(crate) fn masked_roots(_cfg: &Config) -> anyhow::Result<Vec<PathBuf>> {
    Ok(Vec::new())
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
        /// What the injected pasta probe answers.
        pasta: bool,
        /// The Lead listener address the injected MCP plan reports.
        listener: Option<std::net::SocketAddr>,
    }

    fn fixture() -> Fixture {
        // outside /tmp: the Lead's private /tmp swallows masks there
        let tmp = tempfile::Builder::new()
            .prefix(".lead-launch-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        let cfg = Config {
            agents: vec![toml::from_str("name = \"lead-agent\"\ncommand = \"true\"").unwrap()],
            lead: Some(thegn_core::config_lead::LeadConfig {
                agent: "lead-agent".into(),
                notes_dir: Some(root.join("notes")),
                // Today's argv by default; the isolated tests opt in.
                network: LeadNetwork::Host,
                ..Default::default()
            }),
            ..Config::default()
        };
        Fixture {
            _tmp: tmp,
            root,
            cfg,
            pasta: true,
            listener: None,
        }
    }

    fn isolated(fx: &mut Fixture) {
        if let Some(lead) = fx.cfg.lead.as_mut() {
            lead.network = LeadNetwork::Isolated;
        }
        fx.listener = Some("127.0.0.1:4242".parse().unwrap());
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
        let pasta = fx.pasta;
        let pasta_available = move || pasta;
        let roots = move || -> anyhow::Result<Vec<PathBuf>> { Ok(repos.clone()) };
        let dirs = move |_: &Config| control.clone();
        let listener = fx.listener;
        let mcp =
            move |_: &Config, _: &Path, _: &str| -> anyhow::Result<crate::lead::mcp::LeadMcpPlan> {
                Ok(crate::lead::mcp::LeadMcpPlan {
                    control_addr: listener,
                    ..Default::default()
                })
            };
        launch_spec_with(
            &fx.cfg,
            mode,
            &LeadHost {
                bwrap_available: &available,
                pasta_available: &pasta_available,
                repo_roots: &roots,
                control_dirs: &dirs,
                mcp: &mcp,
                daemon_owned: true,
                state_home: fx.root.join("state"),
                data_home: fx.root.join("data"),
                cache_home: fx.root.join("cache"),
                home: Some(fx.root.join("home")),
                thegn_dir: fx.root.join("thegn-home"),
                config_path: fx.root.join("config/thegn/config.toml"),
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
        let got = readable_under_masks(
            &[mask.join("token"), outside],
            std::slice::from_ref(&mask),
            &[],
        )
        .unwrap();
        assert_eq!(got, vec![mask.join("token")]);
        // A directory under a mask re-exposes only the regular files
        // directly inside it that `keep` names, never its subdirectories
        // and never another file (another Lead's token).
        std::fs::write(mask.join("sub/mcp.json"), "{}").unwrap();
        std::fs::write(mask.join("sub/other.json"), "{}").unwrap();
        std::fs::create_dir_all(mask.join("sub/nested")).unwrap();
        assert_eq!(
            readable_under_masks(
                &[mask.join("sub")],
                std::slice::from_ref(&mask),
                &["mcp.json"]
            )
            .unwrap(),
            vec![mask.join("sub/mcp.json")]
        );
        assert!(
            readable_under_masks(&[mask.join("sub")], std::slice::from_ref(&mask), &[])
                .unwrap()
                .is_empty(),
            "a file not in `keep` is never re-exposed"
        );
        if crate::platform::test_symlink_supported() {
            crate::platform::test_symlink(&mask.join("token"), &mask.join("link")).unwrap();
            assert!(readable_under_masks(&[mask.join("link")], &[mask], &[]).is_ok());
        }
    }

    #[test]
    fn isolated_without_pasta_bails_and_never_returns_a_host_argv() {
        let mut fx = fixture();
        isolated(&mut fx);
        fx.pasta = false;
        let err = run(&fx, true, vec![], vec![]).unwrap_err();
        assert_eq!(err.to_string(), NO_PASTA);
        assert!(err.to_string().contains("network = \"host\""));
        assert!(
            !fx.root.join("notes").exists(),
            "a refused launch creates nothing"
        );
        let err = headless(&fx, "hi", true).err().expect("refused");
        assert_eq!(err.to_string(), NO_PASTA);
    }

    #[test]
    fn isolated_without_a_listener_address_bails() {
        let mut fx = fixture();
        isolated(&mut fx);
        fx.listener = None;
        assert!(run(&fx, true, vec![], vec![]).is_err());
    }

    #[test]
    fn isolated_runs_bwrap_inside_pasta_forwarding_only_the_listener() {
        let mut fx = fixture();
        isolated(&mut fx);
        let spec = run(&fx, true, vec![], vec![]).unwrap();
        let pasta = spec.argv.iter().position(|a| a == "pasta").expect("pasta");
        let bwrap = spec.argv.iter().position(|a| a == "bwrap").expect("bwrap");
        assert!(pasta < bwrap, "{:?}", spec.argv);
        assert!(has(&spec.argv, &["-T", "4242"]));
        assert_eq!(
            spec.argv[bwrap - 1],
            "--",
            "pasta's `--` immediately precedes bwrap"
        );
        assert!(!spec.argv.iter().any(|a| a == "--map-host-loopback"));
        assert_eq!(spec.backend, "bwrap");
        assert!(has(&spec.argv, &["--cap-drop", "ALL"]), "{:?}", spec.argv);
        if let Some((uid, gid)) = crate::platform::current_ids() {
            assert!(has(
                &spec.argv,
                &["--uid", &uid.to_string(), "--gid", &gid.to_string()]
            ));
        }
        // The generated resolv.conf is bound over the host's.
        let resolv = fx.root.join("state/thegn/lead-net/resolv.conf");
        assert_eq!(
            std::fs::read_to_string(&resolv).unwrap(),
            thegn_core::lead_net::resolv_conf()
        );
        let resolv_s = resolv.display().to_string();
        assert!(has(
            &spec.argv,
            &["--ro-bind", &resolv_s, "/etc/resolv.conf"]
        ));
        // The headless spec follows the same path.
        let wake = headless(&fx, "go", true).unwrap().spec;
        let p = wake.argv.iter().position(|a| a == "pasta").expect("pasta");
        let b = wake.argv.iter().position(|a| a == "bwrap").expect("bwrap");
        assert!(p < b && has(&wake.argv, &["-T", "4242"]));
    }

    #[test]
    fn host_network_yields_the_plain_bwrap_argv() {
        let fx = fixture();
        let spec = run(&fx, true, vec![], vec![]).unwrap();
        assert!(!spec.argv.iter().any(|a| a == "pasta"), "{:?}", spec.argv);
        assert!(!spec.argv.iter().any(|a| a == "/etc/resolv.conf"));
        assert!(spec.argv.iter().any(|a| a == "bwrap"));
        assert!(
            !spec.argv.iter().any(|a| a == "--uid"),
            "host mode keeps the caller's ids"
        );
        // Host mode never consults the pasta probe.
        let mut fx = fixture();
        fx.pasta = false;
        assert!(run(&fx, true, vec![], vec![]).is_ok());
    }

    #[test]
    fn the_state_dir_is_masked_and_the_notes_dir_bound_after_it() {
        let mut fx = fixture();
        let state = fx.root.join("state/thegn");
        let notes = state.join("lead");
        if let Some(lead) = fx.cfg.lead.as_mut() {
            lead.notes_dir = Some(notes.clone());
        }
        std::fs::create_dir_all(fx.root.join("data/thegn")).unwrap();
        let spec = run(&fx, true, vec![], vec![]).unwrap();
        let state_s = state.display().to_string();
        let notes_s = notes.display().to_string();
        let data_s = fx.root.join("data/thegn").display().to_string();
        let pos = |needle: &[&str]| {
            spec.argv
                .windows(needle.len())
                .position(|w| w.iter().map(String::as_str).eq(needle.iter().copied()))
                .unwrap_or_else(|| panic!("{needle:?} not in {:?}", spec.argv))
        };
        let mask = pos(&["--tmpfs", &state_s]);
        pos(&["--tmpfs", &data_s]);
        let bind = pos(&["--bind", &notes_s, &notes_s]);
        assert!(mask < bind, "the notes dir is re-exposed after the mask");
        assert!(
            has(
                &spec.argv,
                &[
                    "--tmpfs",
                    &fx.root.join("cache/thegn").display().to_string()
                ]
            ),
            "a missing thegn dir is created and masked"
        );
        assert!(fx.root.join("cache/thegn").is_dir());
        assert!(
            !fx.root
                .join("home")
                .join(thegn_core::migrate_brand::LEGACY_HOME_DIR_NAME)
                .exists(),
            "a missing legacy dir is neither created nor masked"
        );
    }

    #[test]
    fn the_thegn_home_is_masked_and_repos_beneath_it_come_back_read_only() {
        let fx = fixture();
        let thegn_home = fx.root.join("thegn-home");
        std::fs::create_dir_all(thegn_home.join("ssh")).unwrap();
        let repo = thegn_home.join("worktrees/r");
        std::fs::create_dir_all(&repo).unwrap();
        let spec = run(&fx, true, vec![repo.clone()], vec![]).unwrap();
        let home_s = thegn_home.display().to_string();
        let repo_s = repo.display().to_string();
        let pos = |needle: &[&str]| {
            spec.argv
                .windows(needle.len())
                .position(|w| w.iter().map(String::as_str).eq(needle.iter().copied()))
                .unwrap_or_else(|| panic!("{needle:?} not in {:?}", spec.argv))
        };
        let mask = pos(&["--tmpfs", &home_s]);
        let ro = pos(&["--ro-bind", &repo_s, &repo_s]);
        assert!(mask < ro, "the repo is re-exposed after the mask");
        assert!(
            !spec
                .argv
                .windows(2)
                .any(|w| w[0] == "--bind" && w[1] == repo_s),
            "a repo is never writable"
        );
    }

    #[test]
    fn the_config_dir_is_masked_with_only_config_toml_and_file_secrets_hidden() {
        let mut fx = fixture();
        let config_dir = fx.root.join("config/thegn");
        std::fs::create_dir_all(config_dir.join("nix-signing")).unwrap();
        std::fs::write(config_dir.join("config.toml"), "").unwrap();
        let secret = fx.root.join("sec/tok");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "lin_x").unwrap();
        fx.cfg.issues.issue_accounts = vec![
            toml::from_str(&format!(
                "name = \"a\"\nprovider = \"linear\"\ntoken = \"file:{}\"",
                secret.display()
            ))
            .unwrap(),
        ];
        let spec = run(&fx, true, vec![], vec![]).unwrap();
        let dir_s = config_dir.display().to_string();
        let file_s = config_dir.join("config.toml").display().to_string();
        let secret_s = secret.display().to_string();
        assert!(
            has(&spec.argv, &["--ro-bind", "/dev/null", &secret_s]),
            "{:?}",
            spec.argv
        );
        let pos = |needle: &[&str]| {
            spec.argv
                .windows(needle.len())
                .position(|w| w.iter().map(String::as_str).eq(needle.iter().copied()))
                .unwrap_or_else(|| panic!("{needle:?} not in {:?}", spec.argv))
        };
        let mask = pos(&["--tmpfs", &dir_s]);
        let file = pos(&["--ro-bind", &file_s, &file_s]);
        assert!(mask < file, "config.toml is re-exposed after the mask");
    }

    #[test]
    fn push_and_inbox_secrets_are_unset_or_hidden() {
        let mut fx = fixture();
        let inbox = fx.root.join("sec/inbox");
        std::fs::create_dir_all(inbox.parent().unwrap()).unwrap();
        std::fs::write(&inbox, "k").unwrap();
        let push = &mut fx.cfg.notifications.push;
        push.token = "env:NTFY_TOKEN".into();
        push.inbox.inbox_secret = format!("file:{}", inbox.display());
        push.sinks = vec![thegn_core::config_push::PushSinkConfig {
            token: "env:SINK_TOKEN".into(),
            ..Default::default()
        }];
        let spec = run(&fx, true, vec![], vec![]).unwrap();
        assert!(
            has(&spec.argv, &["--unsetenv", "NTFY_TOKEN"]),
            "{:?}",
            spec.argv
        );
        assert!(has(&spec.argv, &["--unsetenv", "SINK_TOKEN"]));
        let inbox_s = inbox.display().to_string();
        assert!(has(&spec.argv, &["--ro-bind", "/dev/null", &inbox_s]));
    }

    #[test]
    fn harness_homes_are_masked_while_the_private_home_is_bound() {
        let mut fx = fixture();
        set_agent(&mut fx, "name = \"lead-agent\"\ncommand = \"pi\"");
        let home = fx.root.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".pi")).unwrap();
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        let spec = run(&fx, true, vec![], vec![]).unwrap();
        let s = |p: PathBuf| p.display().to_string();
        let claude = s(home.join(".claude"));
        let pi = s(home.join(".pi"));
        let claude_json = s(home.join(".claude.json"));
        let private = s(fx.root.join("state/thegn/lead-harness/pi"));
        let state = s(fx.root.join("state/thegn"));
        assert!(has(&spec.argv, &["--tmpfs", &claude]), "{:?}", spec.argv);
        assert!(has(&spec.argv, &["--tmpfs", &pi]));
        assert!(has(&spec.argv, &["--tmpfs", &state]));
        assert!(has(&spec.argv, &["--ro-bind", "/dev/null", &claude_json]));
        let mask = spec
            .argv
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == state)
            .unwrap();
        let bind = spec
            .argv
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == private && w[2] == private)
            .expect("private harness home bound");
        assert!(mask < bind);
        assert!(
            !spec
                .argv
                .windows(2)
                .any(|w| w[0] == "--bind" && (w[1] == claude || w[1] == pi)),
            "the shared harness homes are never writable"
        );
    }

    #[test]
    fn launch_unsets_the_display() {
        let fx = fixture();
        let spec = run(&fx, true, vec![], vec![]).unwrap();
        for k in ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"] {
            assert!(has(&spec.argv, &["--unsetenv", k]), "{k}");
        }
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
