//! `thegn vercel-exec <id> -- cmd…`: the Vercel Sandbox exec self-bridge (the
//! `sprite-exec` role for a sandbox with no vendor CLI and no ssh). Capture
//! mode (non-tty stdin) runs the command and relays its output without ever
//! resuming a stopped sandbox; tty mode resumes and streams the
//! `/interactive` PTY, falling back to stream mode. The token is resolved
//! in-process, never passed in argv or the environment.
//!
//! - **Capture** (stdin not a tty): chrome git/fs reads. `run_exec` under
//!   [`Wake::Never`]; a stopped sandbox exits [`EXIT_STOPPED`] with
//!   [`STOPPED_LINE`] on stderr, so a read never buys compute. No keep-alive.
//! - **TTY** (panes, daemon headless sessions): raw mode, `open_exec` (the
//!   `/interactive` PTY), else `open_stream` with [`STREAM_NOTICE`] once.
//!   While the session runs, the `thegn-vx-keep` thread extends the Vercel
//!   session timeout when due; it is the only extension timer anywhere.
//!   Vercel has no reattach: a stream lost without an exit means the remote
//!   process is gone, so the bridge exits [`EXIT_STOPPED`] (75) with one line.
//!
//! The exec environment is `TERM` plus [`ENV_ALLOWLIST`] only: no token, no
//! `VERCEL_*`, no other host variable reaches the sandbox.

use std::io::{IsTerminal as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use termwiz::caps::{Capabilities, ProbeHints};
use termwiz::input::InputEvent;
use termwiz::terminal::{Terminal, new_terminal};
use thegn_core::config::{Config, EnvConfig, EnvProviderKind, ModelCredentials};
use thegn_core::vercel::api::SessionStatus;
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec};
use thegn_svc::vercel::{VercelProvider, Wake};

use crate::sprite_bridge::{FromTty, RemoteModes, encode_input, pty_size};

/// Exit code for a stopped sandbox (capture) and a lost stream (tty):
/// `EX_TEMPFAIL`, the same code `sprite-exec` uses for a lost stream, which
/// `session list` renders as `cause:remote-exec-lost`.
pub(crate) const EXIT_STOPPED: i32 = 75;

/// The one line for a capture call against a stopped sandbox.
pub(crate) const STOPPED_LINE: &str =
    "vercel sandbox stopped (open a pane or run `thegn env provision` to resume)";

/// The one line printed when `/interactive` is unavailable.
pub(crate) const STREAM_NOTICE: &str =
    "interactive PTY unavailable on this Vercel sandbox; running in stream mode";

/// Host variables (besides `TERM`) that may enter the sandbox exec env.
pub(crate) const ENV_ALLOWLIST: &[&str] = &["COLORTERM", "LANG", "LC_ALL", "LC_CTYPE"];

/// `TERM` when the host has none.
const DEFAULT_TERM: &str = "xterm-256color";

/// Exit code when the local terminal went away and no remote exit arrived.
const EXIT_LOCAL_CLOSED: i32 = 1;

/// How long to wait for the remote exit after a local close.
const LOCAL_CLOSE_GRACE: Duration = Duration::from_secs(5);

/// The bound on removing this bridge's login dir at TTY exit.
const REMOVE_LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `cmd` inside Vercel sandbox `id`. Empty `cmd` is a login shell.
pub fn run(cfg: &Config, id: &str, cmd: &[String]) -> Result<()> {
    anyhow::ensure!(
        thegn_core::vercel::admit::valid_sandbox_name(id),
        "vercel-exec: invalid sandbox name"
    );
    let argv: Vec<String> = if cmd.is_empty() {
        vec![
            "/bin/sh".into(),
            "-lc".into(),
            "exec ${SHELL:-/bin/sh} -l".into(),
        ]
    } else {
        cmd.to_vec()
    };
    let env = exec_env(|key| std::env::var(key).ok());
    if std::io::stdin().is_terminal() {
        // TTY only: resolve the env this sandbox belongs to (ledger owner ->
        // its bound env, else the only vercel env) for its provider and mode.
        let (resolved, warning) = resolve_env(cfg, id);
        if let Some(line) = warning {
            note(&line);
        }
        let (provider, upload) = match resolved {
            Some(name) => {
                let envc = cfg.env.get(&name).ok_or_else(|| refusal(id, cfg))?;
                let provider = crate::provider_factory::vercel_provider_for(&envc.provider, id)
                    .ok_or_else(|| unavailable_env(&name))?;
                (provider, upload_plan(cfg, envc))
            }
            None => {
                let provider = first_vercel_provider(cfg, id)?;
                let envc = only_vercel_env(cfg).ok_or_else(|| refusal(id, cfg))?;
                (provider, upload_plan(cfg, envc))
            }
        };
        let code = run_tty(Arc::new(provider), id, argv, env, upload)?;
        std::process::exit(host_exit_code(code));
    }
    // Capture follows the same ledger-aware resolution as TTY mode. The
    // fallback is safe only when there is exactly one configured Vercel env.
    let (resolved, _) = resolve_env(cfg, id);
    let provider = match resolved {
        Some(name) => {
            let envc = cfg.env.get(&name).ok_or_else(|| refusal(id, cfg))?;
            crate::provider_factory::vercel_provider_for(&envc.provider, id)
                .ok_or_else(|| unavailable_env(&name))?
        }
        None => first_vercel_provider(cfg, id)?,
    };
    let code = run_capture(&provider, id, &argv, &env)?;
    std::process::exit(host_exit_code(code));
}

/// The only `provider = "vercel"` env with a resolvable token. This fallback
/// is deliberately refused when the config has zero or multiple Vercel envs.
fn first_vercel_provider(cfg: &Config, id: &str) -> Result<VercelProvider> {
    let envc = only_vercel_env(cfg).ok_or_else(|| refusal(id, cfg))?;
    crate::provider_factory::vercel_provider_for(&envc.provider, id)
        .ok_or_else(|| unavailable_env("the only configured Vercel env"))
}

fn vercel_env_names(cfg: &Config) -> Vec<String> {
    cfg.env
        .iter()
        .filter(|(_, envc)| is_vercel(envc))
        .map(|(name, _)| name.clone())
        .collect()
}

fn only_vercel_env(cfg: &Config) -> Option<&EnvConfig> {
    let mut envs = cfg.env.iter().filter(|(_, envc)| is_vercel(envc));
    let only = envs.next()?.1;
    envs.next().is_none().then_some(only)
}

fn refusal(id: &str, cfg: &Config) -> anyhow::Error {
    let names = vercel_env_names(cfg);
    anyhow!(
        "vercel-exec: cannot tell which vercel env owns {id}; {} Vercel envs are configured ({})",
        names.len(),
        names.join(", ")
    )
}

fn unavailable_env(name: &str) -> anyhow::Error {
    anyhow!("vercel-exec: selected env '{name}' has no resolvable Vercel provider")
}

fn is_vercel(envc: &EnvConfig) -> bool {
    matches!(
        EnvProviderKind::of(&envc.provider.provider),
        EnvProviderKind::Vercel
    )
}

/// One line on stderr (the bridge's only user-facing channel besides the PTY).
fn note(line: &str) {
    let mut err = std::io::stderr();
    if writeln!(err, "{line}").is_err() {
        tracing::debug!(target: "thegn::provider", "vercel-exec: stderr gone");
    }
}

/// The env for sandbox `id`, plus a warning line when none can be picked.
fn resolve_env(cfg: &Config, id: &str) -> (Option<String>, Option<String>) {
    let vercel: Vec<String> = cfg
        .env
        .iter()
        .filter(|(_, envc)| is_vercel(envc))
        .map(|(name, _)| name.clone())
        .collect();
    let owner_env = if vercel.len() > 1 {
        ledger_owner_env(id)
    } else {
        None
    };
    let any_upload = cfg
        .env
        .values()
        .any(|envc| is_vercel(envc) && envc.provider.model_credentials == ModelCredentials::Upload);
    match pick_env(owner_env.as_deref(), &vercel) {
        Some(name) => (Some(name), None),
        None if !any_upload => (None, None),
        None => (
            None,
            Some(
                "model login not uploaded: cannot tell which vercel env owns this sandbox; \
                 continuing without it"
                    .to_string(),
            ),
        ),
    }
}

/// The bound env of the worktree that owns sandbox `id` in the ledger. Any
/// failed read is `None` (the caller then falls back).
fn ledger_owner_env(id: &str) -> Option<String> {
    use thegn_core::store::WorkspaceStore as _;
    let record = thegn_svc::vps::registry::read(id)?;
    if record.owner.is_empty() {
        return None;
    }
    let db = thegn_core::db::Db::open_read_only_current().ok()??;
    let rows = db.worktrees().ok()?;
    let row = rows.iter().find(|w| {
        !w.worktree.is_empty()
            && thegn_svc::vps::registry::worktree_owner(w.worktree.as_bytes()) == record.owner
    })?;
    db.effective_env(&row.worktree, &row.repo_root)
}

/// What a TTY open uploads under `model_credentials = "upload"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UploadPlan {
    pub agents: Vec<String>,
    pub user_domains: Vec<String>,
    pub install_agents: bool,
    /// `[env.<n>.provider] login_homes`: dedicated login dirs read instead of
    /// the live host login.
    pub login_homes: std::collections::BTreeMap<String, String>,
}

/// The upload plan for `envc`, or `None` unless its mode is `upload`.
fn upload_plan(cfg: &Config, envc: &EnvConfig) -> Option<UploadPlan> {
    if envc.provider.model_credentials != ModelCredentials::Upload {
        return None;
    }
    let overlay = envc.sandbox.home.as_ref().and_then(|h| h.agents.as_deref());
    let agents = upload_agents(overlay, &cfg.sandbox.home.agents, || {
        crate::agent::provisioned_agent_kinds(cfg)
    });
    let user_domains = envc
        .sandbox
        .network_allow
        .clone()
        .unwrap_or_else(|| cfg.sandbox.network_allow.clone());
    Some(UploadPlan {
        install_agents: false, // the TTY open never installs; provisioning sets its own policy
        agents,
        user_domains,
        login_homes: envc.provider.login_homes.clone(),
    })
}

/// Read the host logins, resume, then narrow egress and upload. The sandbox
/// is resumed here (the TTY open would resume it anyway), through the same
/// lifetime-ceiling check an interactive open uses: past `max_lifetime_secs`
/// nothing is resumed and nothing is uploaded (the caller prints one line).
async fn upload_login(
    provider: &VercelProvider,
    id: &str,
    plan: &UploadPlan,
) -> Result<thegn_svc::vercel::login::LoginHandle> {
    let (payloads, skipped) =
        crate::vercel_login::read_host_logins(&plan.agents, &plan.login_homes);
    for line in &skipped {
        tracing::debug!(target: "thegn::provider", skipped = %line, "vercel login agent skipped");
    }
    if payloads.is_empty() {
        let why = if skipped.is_empty() {
            "no claude or codex agent is configured".to_string()
        } else {
            skipped.join("; ")
        };
        anyhow::bail!("{why}");
    }
    provider.resume_interactive(id).await?;
    provider
        .ensure_login(id, &payloads, &plan.user_domains, plan.install_agents)
        .await
}

/// Remove this bridge's login dir, bounded by [`REMOVE_LOGIN_TIMEOUT`].
fn remove_login_bounded(
    rt: &tokio::runtime::Runtime,
    provider: &VercelProvider,
    id: &str,
    login: Option<&thegn_svc::vercel::login::LoginHandle>,
) {
    if let Some(handle) = login {
        let removed = rt.block_on(async {
            tokio::time::timeout(REMOVE_LOGIN_TIMEOUT, provider.remove_login(id, handle)).await
        });
        // best-effort: the dir is tmpfs and dies with the session anyway.
        match removed {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(target: "thegn::provider", error = %e, "vercel login dir removal failed");
            }
            Err(_) => {
                tracing::debug!(target: "thegn::provider", "vercel login dir removal timed out");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure decisions (unit-tested).
// ---------------------------------------------------------------------------

/// The sandbox exec env: `TERM` (defaulted) plus the allowlisted keys that are
/// set, non-empty and free of NUL/newlines. Nothing else.
pub(crate) fn exec_env(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let clean = |v: String| (!v.is_empty() && !v.contains(['\0', '\n', '\r'])).then_some(v);
    let term = get("TERM")
        .and_then(clean)
        .unwrap_or_else(|| DEFAULT_TERM.to_string());
    let mut env = vec![("TERM".to_string(), term)];
    for key in ENV_ALLOWLIST {
        if let Some(value) = get(key).and_then(clean) {
            env.push(((*key).to_string(), value));
        }
    }
    // The provider's one allowlist is the authority (the provider applies it
    // again, with the token ref name, on every path into the sandbox).
    env.retain(|(key, _)| thegn_svc::vercel::env_key_allowed(key));
    env
}

/// The env to use: the ledger owner's bound env when it is a vercel env,
/// else the only vercel env, else `None` (ambiguous: upload is skipped).
pub(crate) fn pick_env(owner_env: Option<&str>, vercel_envs: &[String]) -> Option<String> {
    if let Some(owner) = owner_env
        && vercel_envs.iter().any(|name| name == owner)
    {
        return Some(owner.to_string());
    }
    match vercel_envs {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// `[env.<name>.sandbox.home] agents`, else `[sandbox.home] agents`, else the
/// `[[agents]]` picker kinds.
pub(crate) fn upload_agents(
    overlay: Option<&[String]>,
    global: &[String],
    picker: impl FnOnce() -> Vec<String>,
) -> Vec<String> {
    match overlay {
        Some(agents) if !agents.is_empty() => agents.to_vec(),
        _ if !global.is_empty() => global.to_vec(),
        _ => picker(),
    }
}

/// The exec env plus the login env pairs, only when an upload was planned.
pub(crate) fn with_login_env(
    mut env: Vec<(String, String)>,
    planned: bool,
    login: Option<&thegn_svc::vercel::login::LoginHandle>,
) -> Vec<(String, String)> {
    if planned && let Some(handle) = login {
        env.extend(handle.env.iter().cloned());
    }
    env
}

/// Whether a session in `status` cannot run a capture command without a
/// resume (everything but a live or starting session).
pub(crate) fn needs_resume(status: SessionStatus) -> bool {
    !matches!(status, SessionStatus::Running | SessionStatus::Pending)
}

/// A remote exit code as a host exit status: 0..=255 pass through; anything
/// else (a negative or out-of-range value) is the unknown sentinel, never 0.
pub(crate) fn host_exit_code(code: i32) -> i32 {
    if (0..=255).contains(&code) {
        code
    } else {
        thegn_svc::provider::EXEC_EXIT_STATUS_UNKNOWN
    }
}

/// The keep-alive due calculation: the core policy's next extension.
#[cfg(test)]
pub(crate) fn keep_alive_due(
    now_ms: i64,
    expires_at_ms: i64,
    created_at_ms: i64,
    lifetime_cap_secs: u64,
) -> Option<u64> {
    thegn_core::vercel::policy::next_extension(
        now_ms,
        expires_at_ms,
        created_at_ms,
        lifetime_cap_secs,
    )
}

/// The keep-alive cadence.
pub(crate) fn keep_alive_interval() -> Duration {
    Duration::from_secs(thegn_core::vercel::policy::EXTEND_CHECK_SECS)
}

/// The one line reporting where the interactive PTY went: the admitted
/// ticket host and its `/health` host, or the refused host. Hosts only (the
/// provider never hands back the URL query or the ticket). `None` when no
/// ticket was admitted (nothing to report).
pub(crate) fn interactive_host_line(
    hosts: Option<&thegn_svc::vercel::pty::InteractiveHosts>,
    admitted: bool,
) -> Option<String> {
    let hosts = hosts?;
    Some(if admitted {
        format!(
            "vercel: interactive host {} (health {})",
            hosts.ticket_host, hosts.health_host
        )
    } else {
        format!(
            "vercel: interactive host {} refused; using stream mode",
            hosts.ticket_host
        )
    })
}

/// The one line written when the stream is lost for good.
pub(crate) fn lost_line(id: &str) -> String {
    format!(
        "thegn: vercel sandbox {id} exec stream lost; Vercel cannot reattach, so the remote process did not survive"
    )
}

/// Stream mode has no remote PTY: what one chunk of local raw-mode input
/// becomes. `send` goes to the command's stdin (CR becomes LF), `echo` is
/// written locally (the remote does not echo), and `interrupt` is a Ctrl-C.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamInput {
    pub(crate) send: Vec<u8>,
    pub(crate) echo: Vec<u8>,
    pub(crate) interrupt: bool,
}

pub(crate) fn stream_input(bytes: &[u8]) -> StreamInput {
    let mut out = StreamInput::default();
    // An escape sequence (arrows, function keys) is one key: send, no echo.
    if bytes.first() == Some(&0x1b) {
        out.send = bytes.to_vec();
        return out;
    }
    for &b in bytes {
        match b {
            0x03 => {
                out.interrupt = true;
                return out;
            }
            b'\r' | b'\n' => {
                out.send.push(b'\n');
                out.echo.extend_from_slice(b"\r\n");
            }
            0x7f => {
                out.send.push(b);
                out.echo.extend_from_slice(b"\x08 \x08");
            }
            0x20..=0x7e | 0x80..=0xff | b'\t' => {
                out.send.push(b);
                out.echo.push(b);
            }
            _ => out.send.push(b),
        }
    }
    out
}

/// Stream-mode output on a raw local tty: a bare LF becomes CRLF. `prev_cr`
/// carries whether the previous chunk ended in CR.
pub(crate) fn stream_output(bytes: &[u8], prev_cr: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);
    for &b in bytes {
        if b == b'\n' && !*prev_cr {
            out.push(b'\r');
        }
        out.push(b);
        *prev_cr = b == b'\r';
    }
    out
}

// ---------------------------------------------------------------------------
// Capture mode.
// ---------------------------------------------------------------------------

/// One command, never resuming. Network seam (`cov_ignore`).
fn run_capture(
    provider: &VercelProvider,
    id: &str,
    argv: &[String],
    env: &[(String, String)],
) -> Result<i32> {
    let rt = crate::agent::provider_runtime()?;
    let outcome = rt.block_on(provider.run_exec(id, argv, None, env, Wake::Never));
    let (code, out) = match outcome {
        Ok(done) => done,
        Err(e) => {
            // Classify without resuming: a stopped sandbox is an expected
            // outcome for a chrome read, not an error.
            let stopped = matches!(
                rt.block_on(provider.reaper_view(id)),
                Ok(Some(view)) if needs_resume(view.status)
            );
            if stopped {
                let mut err = std::io::stderr();
                if writeln!(err, "{STOPPED_LINE}").is_err() {
                    tracing::debug!(target: "thegn::provider", "vercel-exec: stderr gone");
                }
                return Ok(EXIT_STOPPED);
            }
            return Err(e);
        }
    };
    let mut stdout = std::io::stdout();
    // EPIPE on a closed `| head` pipe is normal.
    if stdout
        .write_all(out.as_bytes())
        .and_then(|()| stdout.flush())
        .is_err()
    {
        tracing::debug!(target: "thegn::provider", "vercel-exec: stdout closed");
    }
    Ok(code)
}

// ---------------------------------------------------------------------------
// TTY mode (cov_ignore: a live tty + a network stream; the decisions above are
// pure and tested).
// ---------------------------------------------------------------------------

/// Which transport the session runs on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pty,
    Stream,
}

/// How one exec stream ended.
enum StreamEnd {
    Exit(i32),
    Dropped,
    LocalGone,
}

/// The keep-alive thread plus its stop flag. Dropping it (every exit path)
/// sets the flag, wakes the thread and joins it.
struct KeepAlive {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeepAlive {
    /// Start the keep-alive for `id`. Its requests run on `handle`'s runtime
    /// (a multi-thread one, so they progress while the main thread joins).
    fn start(provider: Arc<VercelProvider>, id: String, handle: tokio::runtime::Handle) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let flag = Arc::clone(&stop);
        let thread = thegn_core::thread_name::spawn_named("thegn-vx-keep", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility); // Utility: a 5-minute session-timeout check.
            while !wait_for_stop(&flag, keep_alive_interval()) {
                match handle.block_on(provider.extend_if_due(&id)) {
                    Ok(Some(ms)) => {
                        tracing::debug!(target: "thegn::provider", extended_ms = ms, "vercel session extended");
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(target: "thegn::provider", error = %e, "vercel keep-alive check failed");
                    }
                }
            }
        });
        KeepAlive {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        let (lock, cvar) = &*self.stop;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cvar.notify_all();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::debug!(target: "thegn::provider", "vercel keep-alive thread panicked");
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Wait up to `timeout` for the stop flag; `true` once stop is requested.
fn wait_for_stop(stop: &(Mutex<bool>, Condvar), timeout: Duration) -> bool {
    let (lock, cvar) = stop;
    let guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (guard, _timed_out) = cvar
        .wait_timeout_while(guard, timeout, |stopped| !*stopped)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard
}

/// The tty thread plus its stop handshake; dropping it restores the terminal.
struct TtyGuard {
    done: Arc<AtomicBool>,
    waker: termwiz::terminal::TerminalWaker,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TtyGuard {
    fn stop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if self.waker.wake().is_err() {
            tracing::debug!(target: "thegn::provider", "vercel-exec: tty waker failed");
        }
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            tracing::debug!(target: "thegn::provider", "vercel-exec: tty thread panicked");
        }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The bridge runtime: one worker, so driver tasks and the keep-alive's
/// `Handle::block_on` calls progress whatever the main thread is doing.
fn bridge_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("thegn-vx-rt")
        .enable_all()
        .build()
        .map_err(|e| anyhow!("tokio runtime: {e}"))
}

fn run_tty(
    provider: Arc<VercelProvider>,
    id: &str,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    upload: Option<UploadPlan>,
) -> Result<i32> {
    let hints = ProbeHints::new_from_env()
        .mouse_reporting(Some(false))
        .bracketed_paste(Some(false));
    let caps = Capabilities::new_with_hints(hints).context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    let size = term.get_screen_size().context("screen size")?;
    let (cols, rows) = pty_size(size.cols, size.rows);

    let rt = bridge_runtime()?;
    let done = Arc::new(AtomicBool::new(false));
    // With an upload planned, SIGTERM/SIGHUP are caught from before the first
    // credential byte leaves, so the bridge always reaches the login removal
    // below instead of dying with the dir still in the sandbox.
    if upload.is_some() {
        let _enter = rt.enter();
        crate::platform::install_shutdown_signal(done.clone(), term.waker());
    }
    // `model_credentials = "upload"`: resume, narrow egress, upload, BEFORE the
    // PTY opens. A failure is one line; the session still opens without it.
    let login =
        upload.as_ref().and_then(
            |plan| match rt.block_on(upload_login(&provider, id, plan)) {
                Ok(handle) => Some(handle),
                Err(e) => {
                    let reason = format!("{e:#}").replace(['\n', '\r'], " ");
                    note(&format!(
                        "model login not uploaded: {reason}; continuing without it"
                    ));
                    None
                }
            },
        );
    let spec = ExecSpec {
        argv,
        tty: true,
        cols,
        rows,
        env: with_login_env(env, upload.is_some(), login.as_ref()),
        cwd: None,
    };

    // Open BEFORE raw mode so an open failure never leaves the tty altered.
    let (hosts, opened) = rt.block_on(provider.open_exec_reported(id, &spec));
    if let Some(line) = interactive_host_line(hosts.as_ref(), opened.is_ok()) {
        tracing::info!(target: "thegn::provider", "{line}");
        note(&line);
    }
    let (session, mode) = match opened {
        Ok(session) => (session, Mode::Pty),
        Err(e) => {
            if hosts.is_none() && format!("{e:#}").contains("is not an allowed Vercel host") {
                let line = format!("vercel: interactive host refused by the pin: {e:#}")
                    .replace(['\n', '\r'], " ");
                tracing::info!(target: "thegn::provider", "{line}");
                note(&line);
            }
            tracing::debug!(target: "thegn::provider", error = %e, "vercel /interactive unavailable");
            let session = match rt.block_on(Arc::clone(&provider).open_stream(id, &spec)) {
                Ok(session) => session,
                Err(e) => {
                    remove_login_bounded(&rt, &provider, id, login.as_ref());
                    return Err(e);
                }
            };
            note(STREAM_NOTICE);
            (session, Mode::Stream)
        }
    };

    // Never in capture mode: only a live tty session extends the timeout.
    let mut keep = KeepAlive::start(Arc::clone(&provider), id.to_string(), rt.handle().clone());

    // On error `keep` drops here, which stops and joins it.
    if let Err(e) = term.set_raw_mode().context("raw mode") {
        keep.stop();
        remove_login_bounded(&rt, &provider, id, login.as_ref());
        return Err(e);
    }
    let (tty_tx, tty_rx) = tokio::sync::mpsc::channel::<FromTty>(256);
    let waker = term.waker();
    let thread = {
        let done = done.clone();
        thegn_core::thread_name::spawn_named("thegn-vx-in", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Interactive); // Interactive: remote session keystrokes.
            tty_loop(term, tty_tx, done)
        })
    };
    let mut tty = TtyGuard {
        done,
        waker,
        thread: Some(thread),
    };

    let outcome = rt.block_on(drive(session, tty_rx, mode));
    // Restore the tty first, then stop the keep-alive, then clean up, then report.
    tty.stop();
    keep.stop();
    remove_login_bounded(&rt, &provider, id, login.as_ref());
    // The session may have left commits in the sandbox: flag them for pull.
    crate::vercel_pull_marker::mark(id);
    Ok(match outcome {
        Some(code) => code,
        None => {
            let mut out = std::io::stdout();
            if writeln!(out, "{}", lost_line(id))
                .and_then(|()| out.flush())
                .is_err()
            {
                tracing::debug!(target: "thegn::provider", "vercel-exec: stdout gone");
            }
            EXIT_STOPPED
        }
    })
}

/// Pump the session to its end. `Some(code)` to exit with; `None` when the
/// stream was lost without an exit (no reattach on Vercel).
async fn drive(
    mut session: ExecSession,
    mut tty_rx: tokio::sync::mpsc::Receiver<FromTty>,
    mode: Mode,
) -> Option<i32> {
    match pump(&mut session, &mut tty_rx, mode).await {
        StreamEnd::Exit(code) => Some(code),
        StreamEnd::Dropped => None,
        StreamEnd::LocalGone => {
            if session.control.send(ExecControl::Close).await.is_err() {
                tracing::debug!(target: "thegn::provider", "vercel-exec: driver already gone");
            }
            Some(await_exit(&mut session).await.unwrap_or(EXIT_LOCAL_CLOSED))
        }
    }
}

async fn await_exit(session: &mut ExecSession) -> Option<i32> {
    tokio::time::timeout(LOCAL_CLOSE_GRACE, async {
        while let Some(frame) = session.frames.recv().await {
            if let ExecFrame::Exit(code) = frame {
                return Some(code);
            }
        }
        None
    })
    .await
    .unwrap_or(None)
}

async fn pump(
    session: &mut ExecSession,
    tty_rx: &mut tokio::sync::mpsc::Receiver<FromTty>,
    mode: Mode,
) -> StreamEnd {
    let mut out = std::io::stdout();
    let mut modes = RemoteModes::default();
    let mut prev_cr = false;
    loop {
        tokio::select! {
            frame = session.frames.recv() => match frame {
                Some(ExecFrame::Stdout(bytes)) => {
                    let shown = match mode {
                        Mode::Pty => {
                            modes.observe(&bytes);
                            bytes
                        }
                        Mode::Stream => stream_output(&bytes, &mut prev_cr),
                    };
                    if out.write_all(&shown).and_then(|()| out.flush()).is_err() {
                        return StreamEnd::LocalGone;
                    }
                }
                Some(ExecFrame::Exit(code)) => return StreamEnd::Exit(code),
                None => return StreamEnd::Dropped,
            },
            ev = tty_rx.recv() => match ev {
                Some(FromTty::Resize { cols, rows }) => {
                    if mode == Mode::Pty
                        && session.control.send(ExecControl::Resize { cols, rows }).await.is_err()
                    {
                        tracing::debug!(target: "thegn::provider", "vercel-exec: resize after driver end");
                    }
                }
                Some(ev) => {
                    let Some(bytes) = encode_input(&ev, &modes) else { continue };
                    let send = match mode {
                        Mode::Pty => bytes,
                        Mode::Stream => {
                            let input = stream_input(&bytes);
                            if input.interrupt {
                                // Ctrl-C ends the command (SIGTERM, then SIGKILL).
                                return StreamEnd::LocalGone;
                            }
                            if !input.echo.is_empty()
                                && out.write_all(&input.echo).and_then(|()| out.flush()).is_err()
                            {
                                return StreamEnd::LocalGone;
                            }
                            input.send
                        }
                    };
                    if !send.is_empty()
                        && session.control.send(ExecControl::Stdin(send)).await.is_err()
                    {
                        tracing::debug!(target: "thegn::provider", "vercel-exec: stdin after driver end");
                    }
                }
                None => return StreamEnd::LocalGone,
            },
        }
    }
}

/// Blocking tty reader (copied from `sprite_bridge::tty_loop`): forwards
/// decoded input and resizes, and restores cooked mode as it ends.
fn tty_loop<T: Terminal>(
    mut term: T,
    tx: tokio::sync::mpsc::Sender<FromTty>,
    done: Arc<AtomicBool>,
) {
    while !done.load(Ordering::SeqCst) {
        let ev = match term.poll_input(None) {
            Ok(Some(InputEvent::Key(k))) => FromTty::Key(k),
            Ok(Some(InputEvent::Paste(text))) => FromTty::Paste(text),
            Ok(Some(InputEvent::Resized { cols, rows })) => {
                let (cols, rows) = pty_size(cols, rows);
                FromTty::Resize { cols, rows }
            }
            Ok(Some(_)) | Ok(None) => continue,
            Err(_) => break,
        };
        if tx.blocking_send(ev).is_err() {
            break;
        }
    }
    if term.set_cooked_mode().is_err() {
        tracing::debug!(target: "thegn::provider", "vercel-exec: cooked mode restore failed");
    }
    if term.flush().is_err() {
        tracing::debug!(target: "thegn::provider", "vercel-exec: terminal flush failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_env_is_term_plus_the_allowlist_only() {
        let host = |key: &str| -> Option<String> {
            match key {
                "TERM" => Some("xterm-ghostty".into()),
                "LANG" => Some("en_US.UTF-8".into()),
                "LC_ALL" => Some(String::new()),
                "LC_CTYPE" => Some("bad\nvalue".into()),
                "VERCEL_TOKEN" | "VERCEL_OIDC_TOKEN" => Some("secret".into()),
                "HOME" | "PATH" | "SSH_AUTH_SOCK" => Some("/host".into()),
                _ => None,
            }
        };
        let env = exec_env(host);
        assert_eq!(
            env,
            vec![
                ("TERM".to_string(), "xterm-ghostty".to_string()),
                ("LANG".to_string(), "en_US.UTF-8".to_string()),
            ]
        );
        assert!(
            env.iter()
                .all(|(k, v)| !k.starts_with("VERCEL") && v != "secret")
        );
        for key in ENV_ALLOWLIST {
            assert!(!key.starts_with("VERCEL"), "{key}");
        }
    }

    #[test]
    fn bridge_allowlist_agrees_with_the_provider_allowlist() {
        for key in ENV_ALLOWLIST.iter().copied().chain(["TERM"]) {
            assert!(thegn_svc::vercel::env_key_allowed(key), "{key}");
        }
        for key in [
            "VERCEL_TOKEN",
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "THEGN_API_TOKEN",
        ] {
            assert!(!thegn_svc::vercel::env_key_allowed(key), "{key}");
        }
    }

    #[test]
    fn exec_env_defaults_term() {
        assert_eq!(
            exec_env(|_| None),
            vec![("TERM".to_string(), DEFAULT_TERM.to_string())]
        );
    }

    #[test]
    fn exit_code_and_notice_text_are_fixed() {
        assert_eq!(EXIT_STOPPED, 75);
        assert_eq!(EXIT_STOPPED, crate::sprite_bridge::EXIT_REMOTE_LOST);
        assert_eq!(
            STOPPED_LINE,
            "vercel sandbox stopped (open a pane or run `thegn env provision` to resume)"
        );
        assert_eq!(
            STREAM_NOTICE,
            "interactive PTY unavailable on this Vercel sandbox; running in stream mode"
        );
        let lost = lost_line("tg-a");
        assert!(lost.contains("tg-a") && !lost.contains('\n'), "{lost}");
    }

    #[test]
    fn interactive_host_line_names_hosts_only() {
        use thegn_svc::vercel::pty::InteractiveHosts;
        let hosts = InteractiveHosts {
            ticket_host: "sb-1.vercel.run".into(),
            health_host: "sb-1.vercel.run:443".into(),
        };
        let ok = interactive_host_line(Some(&hosts), true).expect("line");
        assert_eq!(
            ok,
            "vercel: interactive host sb-1.vercel.run (health sb-1.vercel.run:443)"
        );
        let refused = interactive_host_line(Some(&hosts), false).expect("line");
        assert_eq!(
            refused,
            "vercel: interactive host sb-1.vercel.run refused; using stream mode"
        );
        for line in [&ok, &refused] {
            assert!(!line.contains('?') && !line.contains("token"), "{line}");
            assert!(!line.contains('\n'), "{line}");
        }
        assert_eq!(interactive_host_line(None, false), None);
        assert_eq!(interactive_host_line(None, true), None);
    }

    #[test]
    fn only_a_live_session_runs_a_capture_without_resume() {
        assert!(!needs_resume(SessionStatus::Running));
        assert!(!needs_resume(SessionStatus::Pending));
        for status in [
            SessionStatus::Stopped,
            SessionStatus::Stopping,
            SessionStatus::Snapshotting,
            SessionStatus::Failed,
            SessionStatus::Aborted,
            SessionStatus::Unknown,
        ] {
            assert!(needs_resume(status), "{status:?}");
        }
    }

    #[test]
    fn host_exit_code_never_maps_out_of_range_to_success() {
        use thegn_svc::provider::EXEC_EXIT_STATUS_UNKNOWN;
        assert_eq!(host_exit_code(0), 0);
        assert_eq!(host_exit_code(255), 255);
        assert_eq!(host_exit_code(256), EXEC_EXIT_STATUS_UNKNOWN);
        assert_eq!(host_exit_code(-1), EXEC_EXIT_STATUS_UNKNOWN);
        assert_eq!(host_exit_code(i32::MIN), EXEC_EXIT_STATUS_UNKNOWN);
    }

    #[test]
    fn keep_alive_due_follows_the_core_policy() {
        use thegn_core::vercel::policy::{EXTEND_STEP_MS, EXTEND_WHEN_REMAINING_MS};
        let now: i64 = 10_000_000;
        let created = now - 60_000;
        let day = 24 * 3600;
        // Plenty of time left: no extension.
        let far = now + i64::try_from(EXTEND_WHEN_REMAINING_MS).expect("ms") * 2;
        assert_eq!(keep_alive_due(now, far, created, day), None);
        // Under the threshold, far from the lifetime cap: one full step.
        let near = now + 60_000;
        assert_eq!(
            keep_alive_due(now, near, created, day),
            Some(EXTEND_STEP_MS)
        );
        // At the lifetime cap: never extend past it.
        let old = now - i64::try_from(day * 1000).expect("ms");
        assert_eq!(keep_alive_due(now, near, old, day), None);
        assert_eq!(keep_alive_interval(), Duration::from_secs(300));
    }

    #[test]
    fn keep_alive_stop_flag_wakes_the_wait_immediately() {
        let stop = (Mutex::new(false), Condvar::new());
        assert!(
            !wait_for_stop(&stop, Duration::from_millis(1)),
            "times out unset"
        );
        *stop.0.lock().expect("lock") = true;
        let started = std::time::Instant::now();
        assert!(wait_for_stop(&stop, Duration::from_secs(60)));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "a set flag returns at once instead of waiting out the interval"
        );
    }

    #[test]
    fn stream_input_maps_enter_echoes_and_catches_ctrl_c() {
        assert_eq!(
            stream_input(b"ls\r"),
            StreamInput {
                send: b"ls\n".to_vec(),
                echo: b"ls\r\n".to_vec(),
                interrupt: false,
            }
        );
        assert!(stream_input(b"a\x03b").interrupt);
        let esc = stream_input(b"\x1b[A");
        assert_eq!(esc.send, b"\x1b[A".to_vec());
        assert!(esc.echo.is_empty(), "an escape sequence is not echoed");
        let bs = stream_input(b"\x7f");
        assert_eq!(
            (bs.send, bs.echo),
            (b"\x7f".to_vec(), b"\x08 \x08".to_vec())
        );
        let ctl = stream_input(b"\x04");
        assert_eq!((ctl.send, ctl.echo), (b"\x04".to_vec(), Vec::new()));
    }

    #[test]
    fn stream_output_turns_bare_lf_into_crlf_across_chunks() {
        let mut cr = false;
        assert_eq!(stream_output(b"a\nb\r\n", &mut cr), b"a\r\nb\r\n".to_vec());
        assert_eq!(stream_output(b"x\r", &mut cr), b"x\r".to_vec());
        assert!(cr);
        assert_eq!(stream_output(b"\ny", &mut cr), b"\ny".to_vec());
        assert!(!cr);
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn env_pick_prefers_the_ledger_owner_then_a_single_env() {
        let two = names(&["vx-a", "vx-b"]);
        // The ledger owner's bound env wins when it is a vercel env.
        assert_eq!(pick_env(Some("vx-b"), &two).as_deref(), Some("vx-b"));
        // An owner env that is not a vercel env is ignored.
        assert_eq!(pick_env(Some("local"), &two), None);
        // Exactly one vercel env: that one, whatever the owner said.
        let one = names(&["vx-a"]);
        assert_eq!(pick_env(None, &one).as_deref(), Some("vx-a"));
        assert_eq!(pick_env(Some("local"), &one).as_deref(), Some("vx-a"));
        // Ambiguous: skip (no first-env pick).
        assert_eq!(pick_env(None, &two), None);
        assert_eq!(pick_env(None, &[]), None);
    }

    #[test]
    fn capture_mode_with_two_envs_and_no_owner_refuses() {
        let mut cfg = Config::default();
        for name in ["vx-a", "vx-b"] {
            let mut env = EnvConfig::default();
            env.provider.provider = "vercel".into();
            cfg.env.insert(name.into(), env);
        }
        assert!(first_vercel_provider(&cfg, "sb-1").is_err());
        assert!(only_vercel_env(&cfg).is_none());
        let error = refusal("sb-1", &cfg).to_string();
        assert!(error.contains("sb-1"), "{error}");
        assert!(error.contains("vx-a") && error.contains("vx-b"), "{error}");
        assert!(
            !error.contains("token") && !error.contains("secret"),
            "{error}"
        );
    }

    #[test]
    fn capture_mode_with_one_env_uses_it() {
        let mut cfg = Config::default();
        let mut env = EnvConfig::default();
        env.provider.provider = "vercel".into();
        cfg.env.insert("vx-only".into(), env);
        let (resolved, warning) = resolve_env(&cfg, "sb-1");
        assert_eq!(resolved.as_deref(), Some("vx-only"));
        assert!(warning.is_none());
        assert!(only_vercel_env(&cfg).is_some());
    }

    #[test]
    fn upload_agents_prefer_the_env_overlay_then_global_then_picker() {
        let overlay = names(&["codex"]);
        let global = names(&["claude"]);
        let picker = || names(&["pi"]);
        assert_eq!(
            upload_agents(Some(overlay.as_slice()), &global, picker),
            overlay
        );
        assert_eq!(upload_agents(None, &global, picker), global);
        assert_eq!(upload_agents(Some(&[][..]), &[], picker), names(&["pi"]));
    }

    #[test]
    fn mode_none_adds_no_login_env() {
        let base = vec![("TERM".to_string(), "xterm".to_string())];
        let handle = thegn_svc::vercel::login::LoginHandle {
            dir: "/dev/shm/thegn-login-0123456789abcdef".into(),
            env: vec![(
                "CODEX_HOME".into(),
                "/dev/shm/thegn-login-0123456789abcdef/codex".into(),
            )],
        };
        assert_eq!(with_login_env(base.clone(), false, Some(&handle)), base);
        assert_eq!(with_login_env(base.clone(), true, None), base);
        let with = with_login_env(base.clone(), true, Some(&handle));
        assert_eq!(with.len(), 2);
        assert_eq!(with[1].0, "CODEX_HOME");
    }

    #[test]
    fn upload_plan_is_none_unless_the_mode_is_upload() {
        let cfg = Config::default();
        let mut envc = EnvConfig::default();
        envc.provider.provider = "vercel".into();
        assert_eq!(upload_plan(&cfg, &envc), None);
        envc.provider.model_credentials = ModelCredentials::Upload;
        envc.sandbox.network_allow = Some(names(&["example.com"]));
        let plan = upload_plan(&cfg, &envc).expect("upload plans");
        assert_eq!(plan.user_domains, names(&["example.com"]));
        assert!(!plan.install_agents);
    }
}
