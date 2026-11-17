//! `thegn agent run` — spawn a headless agent into a worktree in one command
//! (THE-2005): resolve the brief, run the `session open` offline refusals,
//! open through the same plain-open path (`session::open_plain`), optionally
//! track a dispatch row, and with `--wait` observe the session until it exits
//! (or a timeout), printing the ANSI-stripped tail of its output.
//!
//! The I/O lives in thin async fns; everything decidable without a daemon —
//! the ANSI stripper, the bounded tail ring, the line cap and the exit-code
//! mapping — is a pure fn with unit tests.

use anyhow::Result;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::pipeline_report::display_cell;
use thegn_core::store::WorkspaceStore;
use thegn_core::{msg, outln};
use thegn_svc::control::client::{AttachControl, ControlClient};

/// Bytes of session output `--wait` keeps (a ring: the oldest bytes drop).
pub(crate) const TAIL_BYTES: usize = 256 * 1024;
/// Non-blank lines of the stripped tail that are printed / returned.
pub(crate) const TAIL_LINES: usize = 200;
/// `--timeout` default, seconds.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// Parsed `agent run` flags (the clap variant's fields, moved in).
pub(crate) struct RunArgs {
    pub agent: String,
    pub worktree: String,
    pub env: Option<String>,
    pub brief_file: Option<String>,
    pub prompt: Option<String>,
    pub vars: Vec<String>,
    pub track: Option<String>,
    pub wait: bool,
    pub timeout_secs: u64,
    pub json: bool,
}

/// How a `--wait` ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitStatus {
    /// The session exited; `None` when the daemon could not reap a code.
    Exited(Option<i32>),
    /// The timeout elapsed first; the session was left running.
    Timeout,
}

impl WaitStatus {
    fn label(self) -> &'static str {
        match self {
            WaitStatus::Exited(_) => "exited",
            WaitStatus::Timeout => "timeout",
        }
    }

    fn exit_code(self) -> Option<i32> {
        match self {
            WaitStatus::Exited(code) => code,
            WaitStatus::Timeout => None,
        }
    }
}

/// The process exit code for a `--wait` outcome: 0 on a clean exit, 1 on any
/// other (or unknown) exit code, `EXIT_RETRYABLE` on timeout.
pub(crate) fn process_exit_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(Some(0)) => crate::cmd::EXIT_OK,
        WaitStatus::Exited(_) => crate::cmd::EXIT_ERROR,
        WaitStatus::Timeout => crate::cmd::EXIT_RETRYABLE,
    }
}

/// A byte ring holding at most `cap` of the most recent bytes.
pub(crate) struct TailRing {
    buf: VecDeque<u8>,
    cap: usize,
}

impl TailRing {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if self.cap == 0 {
            return;
        }
        if bytes.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(bytes);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

#[derive(Clone, Copy)]
enum Strip {
    Text,
    Esc,
    EscIntermediate,
    Csi,
    /// OSC / DCS / SOS / PM / APC payload, ended by BEL or ST (`ESC \`).
    Str,
    StrEsc,
}

/// Strip terminal control from raw PTY output: CSI sequences, OSC/DCS-style
/// strings (BEL- or ST-terminated), other two-byte escapes, and C0/C1
/// controls other than `\n` and `\t`. `\r\n` becomes `\n`; a lone `\r`
/// returns to the start of the line, so the text after it replaces the line
/// (how a spinner or progress bar reads once settled). Unicode format
/// characters (bidi overrides and the like) are dropped too. Valid UTF-8 is
/// kept intact; invalid bytes become U+FFFD.
pub(crate) fn strip_ansi(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut line_start = 0usize;
    let mut pending_cr = false;
    let mut st = Strip::Text;
    for &b in bytes {
        if pending_cr {
            pending_cr = false;
            if b == b'\n' {
                out.push(b'\n');
                line_start = out.len();
                continue;
            }
            if b != b'\r' {
                out.truncate(line_start);
            }
        }
        st = match st {
            Strip::Text => match b {
                0x1b => Strip::Esc,
                b'\r' => {
                    pending_cr = true;
                    Strip::Text
                }
                b'\n' => {
                    out.push(b'\n');
                    line_start = out.len();
                    Strip::Text
                }
                b'\t' => {
                    out.push(b'\t');
                    Strip::Text
                }
                0x00..=0x1f | 0x7f => Strip::Text,
                _ => {
                    out.push(b);
                    Strip::Text
                }
            },
            Strip::Esc => match b {
                b'[' => Strip::Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => Strip::Str,
                0x20..=0x2f => Strip::EscIntermediate,
                0x1b => Strip::Esc,
                _ => Strip::Text,
            },
            Strip::EscIntermediate => match b {
                0x20..=0x2f => Strip::EscIntermediate,
                0x1b => Strip::Esc,
                _ => Strip::Text,
            },
            Strip::Csi => match b {
                0x40..=0x7e => Strip::Text,
                0x1b => Strip::Esc,
                _ => Strip::Csi,
            },
            Strip::Str => match b {
                0x07 => Strip::Text,
                0x1b => Strip::StrEsc,
                _ => Strip::Str,
            },
            Strip::StrEsc => match b {
                b'\\' => Strip::Text,
                0x1b => Strip::StrEsc,
                _ => Strip::Str,
            },
        };
    }
    String::from_utf8_lossy(&out)
        .chars()
        .filter(|&c| {
            (c == '\n' || c == '\t' || !c.is_control())
                && !thegn_core::pipeline_report::is_format_control(c)
        })
        .collect()
}

/// The last `max_lines` non-blank lines of `text` (trailing whitespace
/// trimmed), joined with `\n`.
pub(crate) fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// The printed tail of a captured ring: stripped, then line-capped.
pub(crate) fn output_tail(raw: &[u8]) -> String {
    tail_lines(&strip_ansi(raw), TAIL_LINES)
}

/// The env every `agent run` onto an unbound worktree is pinned to.
pub(crate) const PIPELINE_ENV: &str = "pipeline";

/// What `agent run` does with the worktree's env selection before the open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnvChoice {
    /// Pin the worktree to this env (a DB worktree selection, like `env set`).
    Pin(String),
    /// The worktree (or its workspace/repo) already selects an env: keep it.
    Keep,
    /// No selection and no `[env.pipeline]`: the default resolution applies.
    KeepNoPipeline,
}

/// Decide the env pin. `has_selection`: a DB worktree/workspace env, a repo
/// overlay `env = "…"`, or a stored provider location binds the worktree
/// (`[sandbox] default_env` does not).
/// Pure.
pub(crate) fn env_choice(
    explicit: Option<&str>,
    has_selection: bool,
    pipeline_defined: bool,
) -> EnvChoice {
    match explicit {
        Some(name) => EnvChoice::Pin(name.to_string()),
        None if has_selection => EnvChoice::Keep,
        None if pipeline_defined => EnvChoice::Pin(PIPELINE_ENV.to_string()),
        None => EnvChoice::KeepNoPipeline,
    }
}

/// The repo root a worktree's workspace/overlay selection is keyed by.
fn repo_root_of(db: &Db, wt: &str) -> PathBuf {
    db.repo_root_for(wt)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(Path::new(wt)))
        .unwrap_or_else(|| PathBuf::from(wt))
}

/// Apply the `agent run` env pin to `wt` in the real DB. Blocking (the
/// stable-daemon probe builds its own runtime): call off the async runtime.
fn pin_env_blocking(cfg: &Config, wt: &str, explicit: Option<&str>) -> Result<Option<String>> {
    let db = Db::open()?;
    let repo_root = repo_root_of(&db, wt);
    pin_env_for_run(cfg, &db, wt, &repo_root, explicit)
}

/// Pin `wt`'s env for `agent run` per [`env_choice`]. Returns the env pinned
/// by this run (`None` when the existing resolution is kept). Notes go to
/// stderr; stdout belongs to `--json`. Never calls `cmd::env::set` (it prints
/// to stdout and reconciles provider locations): a worktree carrying a
/// provider location is refused instead of re-pinned under it.
pub(crate) fn pin_env_for_run(
    cfg: &Config,
    db: &Db,
    wt: &str,
    repo_root: &Path,
    explicit: Option<&str>,
) -> Result<Option<String>> {
    let explicit = match explicit.map(str::trim) {
        Some("") => anyhow::bail!("--env needs an env name"),
        other => other,
    };
    let repo_s = repo_root.to_string_lossy();
    let selected = db.effective_env_selection(wt, &repo_s)?.name;
    let provider_bound =
        crate::provider_git::is_provider_location(wt, db.location_for(wt)?.as_deref());
    let has_selection =
        selected.is_some() || provider_bound || !cfg.repo_env_name(repo_root).is_empty();
    match env_choice(explicit, has_selection, cfg.env.contains_key(PIPELINE_ENV)) {
        EnvChoice::Keep => Ok(None),
        EnvChoice::KeepNoPipeline => {
            msg::info(&format!(
                "agent run: [env.{PIPELINE_ENV}] is not defined, so {wt} keeps its default env resolution"
            ));
            Ok(None)
        }
        EnvChoice::Pin(name) => {
            if name != "default" && !cfg.env.contains_key(&name) {
                msg::warn(&format!(
                    "environment {name:?} is not defined under [env.{name}]; it will resolve to the default until you add it"
                ));
            }
            crate::cmd::wt::refuse_dev_env_on_stable_daemon(cfg, Some(&name))?;
            if provider_bound && selected.as_deref() != Some(name.as_str()) {
                anyhow::bail!(
                    "{wt} carries a provider location and agent run would pin it to env {name}; \
                     run `thegn env set {name} --worktree {wt}` first"
                );
            }
            db.set_worktree_env(wt, &name)?;
            msg::info(&format!("agent run: worktree {wt} → env {name}"));
            Ok(Some(name))
        }
    }
}

/// Run `thegn agent run`.
pub(crate) fn run(cfg: &Config, args: RunArgs) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let code = rt.block_on(run_async(cfg, args))?;
    if code != crate::cmd::EXIT_OK {
        std::process::exit(code);
    }
    Ok(())
}

async fn run_async(cfg: &Config, args: RunArgs) -> Result<i32> {
    let RunArgs {
        agent,
        worktree,
        env,
        brief_file,
        prompt,
        vars,
        track,
        wait,
        timeout_secs,
        json,
    } = args;
    // Offline refusals first (no daemon needed): the brief, an empty task,
    // and `session open`'s own preflight (the `--track` identity check).
    let prompt = crate::cmd::session_brief::resolve_brief(
        prompt.as_deref().unwrap_or(""),
        brief_file.as_deref(),
        &vars,
    )?;
    if prompt.trim().is_empty() {
        anyhow::bail!("agent run needs a task: give a non-empty --prompt or --brief-file");
    }
    crate::cmd::session::open_preflight(
        cfg,
        None,
        None,
        track.as_deref(),
        Some(&worktree),
        &prompt,
        true,
    )?;
    let client = match crate::cmd::session::connect(cfg).await {
        Ok(c) => c,
        Err(e) => {
            if json {
                crate::cmd::emit_json(&serde_json::json!({ "error": "no_daemon" }))?;
            }
            return Err(e);
        }
    };
    // A headless open onto an unprovisioned provider worktree: provision it
    // first, exactly as `session open` does for a prompted open.
    let wt = crate::cmd::resolve_worktree(Some(worktree.clone()))
        .to_string_lossy()
        .into_owned();
    // Pin the env before provisioning, so provisioning and the open both see
    // it. Off the runtime: the stable-daemon probe blocks on its own runtime.
    let pinned = {
        let cfg = cfg.clone();
        let wt = wt.clone();
        tokio::task::spawn_blocking(move || pin_env_blocking(&cfg, &wt, env.as_deref()))
            .await
            .map_err(|e| anyhow::anyhow!("agent run env pin task failed: {e}"))??
    };
    crate::cmd::env::provision_for_headless_open(cfg, wt).await?;
    let started = Instant::now();
    let (info, row) = crate::cmd::session::open_plain(
        cfg,
        &client,
        crate::cmd::session::PlainOpen {
            agent: Some(agent),
            stage: None,
            worktree: Some(worktree),
            prompt,
            headless: true,
            bind: true,
            adopt: false,
            track: track.as_deref(),
            permissions: Vec::new(),
            widen_permissions: false,
        },
    )
    .await?;
    if !wait {
        if json {
            crate::cmd::emit_json(&serde_json::json!({
                "session": info.id,
                "row": row,
                "env": pinned,
            }))?;
        } else {
            outln!("{}", display_cell(&info.id));
            if let Some(row) = row {
                outln!("dispatch {row} tracked");
            }
        }
        return Ok(crate::cmd::EXIT_OK);
    }
    let (status, raw) =
        observe_until_exit(&client, &info.id, Duration::from_secs(timeout_secs)).await?;
    let duration_ms = thegn_core::time_policy::saturating_i64(started.elapsed().as_millis());
    let tail = output_tail(&raw);
    if json {
        crate::cmd::emit_json(&serde_json::json!({
            "session": info.id,
            "row": row,
            "env": pinned,
            "status": status.label(),
            "exit_code": status.exit_code(),
            "output_tail": tail,
            "duration_ms": duration_ms,
        }))?;
    } else {
        if !tail.is_empty() {
            outln!("{tail}");
        }
        let id = display_cell(&info.id);
        match status {
            WaitStatus::Exited(code) => outln!(
                "[agent run: session {id} exited {}]",
                code.map_or_else(|| "?".to_string(), |c| c.to_string())
            ),
            WaitStatus::Timeout => outln!(
                "[agent run: timed out after {timeout_secs}s; session {id} is still running — \
                 `thegn session wait {id}` or `thegn session close {id}`]"
            ),
        }
        if let Some(row) = row {
            outln!("dispatch {row} tracked");
        }
    }
    Ok(process_exit_code(status))
}

/// Exit code of `agent result` while the session is still running.
pub(crate) const EXIT_RESULT_RUNNING: i32 = crate::cmd::EXIT_NOT_FOUND;
/// How long `agent result` waits for the first output frame.
const RESULT_TAIL_WAIT: Duration = Duration::from_secs(2);

/// Parsed `agent result` flags.
pub(crate) struct ResultArgs {
    pub row: Option<i64>,
    pub session: Option<String>,
    pub json: bool,
}

/// A session's state as `agent result` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultStatus {
    Running,
    Exited(Option<i32>),
    Unknown,
}

impl ResultStatus {
    fn label(self) -> &'static str {
        match self {
            ResultStatus::Running => "running",
            ResultStatus::Exited(_) => "exited",
            ResultStatus::Unknown => "unknown",
        }
    }

    fn exit_code(self) -> Option<i32> {
        match self {
            ResultStatus::Exited(code) => code,
            _ => None,
        }
    }
}

/// Pure: the session id a `--row` names, from the dispatch table.
pub(crate) fn resolve_row_session(db: &Db, row: i64) -> Result<String> {
    let dispatch = db
        .get_dispatch(row)?
        .ok_or_else(|| anyhow::anyhow!("no dispatch row {row}"))?;
    dispatch
        .session_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("dispatch {row} has no session"))
}

/// Pure: classify `session` against the daemon's listing (which keeps
/// tombstones for exited sessions).
pub(crate) fn status_from_sessions(
    sessions: &[thegn_svc::control::SessionInfo],
    session: &str,
) -> ResultStatus {
    match sessions.iter().find(|s| s.id == session) {
        None => ResultStatus::Unknown,
        Some(s) if s.exited_at_ms.is_some() => ResultStatus::Exited(s.exit_code),
        Some(_) => ResultStatus::Running,
    }
}

/// Pure: the process exit code for an `agent result` status.
pub(crate) fn result_exit_code(status: ResultStatus) -> i32 {
    match status {
        ResultStatus::Exited(code) => process_exit_code(WaitStatus::Exited(code)),
        ResultStatus::Running => EXIT_RESULT_RUNNING,
        ResultStatus::Unknown => crate::cmd::EXIT_RETRYABLE,
    }
}

/// Pure: the `--json` object.
pub(crate) fn result_json(
    row: Option<i64>,
    session: &str,
    status: ResultStatus,
    tail: Option<&str>,
    tail_note: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "row": row,
        "session": session,
        "status": status.label(),
        "exit_code": status.exit_code(),
        "output_tail": tail,
        "tail_note": tail_note,
    })
}

pub(crate) fn result(cfg: &Config, args: ResultArgs) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let code = rt.block_on(result_async(cfg, args))?;
    if code != crate::cmd::EXIT_OK {
        std::process::exit(code);
    }
    Ok(())
}

/// One attach, the first output frame within [`RESULT_TAIL_WAIT`], detach.
/// Never waits for exit. `Err` carries the `tail_note`.
async fn first_output(client: &ControlClient, session: &str) -> Result<Vec<u8>, String> {
    use thegn_core::control_wire::EventFrame;
    let client_id = format!("agent-result-{}", std::process::id());
    let mut stream = client
        .attach(session, &client_id, 0, 0, true)
        .await
        .map_err(|e| format!("attach failed: {e}"))?;
    let got = tokio::time::timeout(RESULT_TAIL_WAIT, async {
        while let Some(frame) = stream.frames.recv().await {
            if let EventFrame::PaneSnapshot { bytes, .. } | EventFrame::PaneDelta { bytes, .. } =
                frame
            {
                return Some(bytes);
            }
        }
        None
    })
    .await;
    detach(&stream.control).await;
    match got {
        Ok(Some(bytes)) => Ok(bytes.to_vec()),
        Ok(None) => Err("the session sent no output frame".to_string()),
        Err(_) => Err("no output frame within 2s".to_string()),
    }
}

async fn result_async(cfg: &Config, args: ResultArgs) -> Result<i32> {
    let ResultArgs { row, session, json } = args;
    let session = match (row, session) {
        (Some(row), _) => resolve_row_session(&Db::open()?, row)?,
        (None, Some(session)) => session,
        (None, None) => anyhow::bail!("agent result needs --row or --session"),
    };
    let client = match crate::cmd::session::connect(cfg).await {
        Ok(c) => c,
        Err(e) => {
            if json {
                crate::cmd::emit_json(&serde_json::json!({ "error": "no_daemon" }))?;
            }
            return Err(crate::cmd::Retryable(e).into());
        }
    };
    let status = status_from_sessions(&client.sessions().await?, &session);
    let (tail, tail_note) = if status == ResultStatus::Unknown {
        (
            None,
            Some(format!("session {session} is not known to the daemon")),
        )
    } else {
        match first_output(&client, &session).await {
            Ok(raw) => (Some(output_tail(&raw)), None),
            Err(note) => (None, Some(note)),
        }
    };
    if json {
        crate::cmd::emit_json(&result_json(
            row,
            &session,
            status,
            tail.as_deref(),
            tail_note.as_deref(),
        ))?;
    } else {
        if let Some(tail) = tail.as_deref().filter(|t| !t.is_empty()) {
            outln!("{tail}");
        }
        if let Some(note) = &tail_note {
            outln!("[agent result: {note}]");
        }
        let code = status
            .exit_code()
            .map_or_else(String::new, |c| format!(" {c}"));
        outln!(
            "[agent result: session {} {}{code}]",
            display_cell(&session),
            status.label()
        );
    }
    Ok(result_exit_code(status))
}

/// Detach the observer. Best-effort: a closed channel means the stream already
/// ended, which is what detaching asks for.
async fn detach(control: &tokio::sync::mpsc::Sender<AttachControl>) {
    if control.send(AttachControl::Close).await.is_err() {
        tracing::debug!("agent run: attach stream already closed");
    }
}

/// Observe `session` (an observer attach: never resizes, never types) until
/// it exits or `timeout` elapses, keeping the last [`TAIL_BYTES`] of output.
/// On timeout the observer detaches and the session keeps running.
async fn observe_until_exit(
    client: &ControlClient,
    session: &str,
    timeout: Duration,
) -> Result<(WaitStatus, Vec<u8>)> {
    use thegn_core::control_wire::EventFrame;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut ring = TailRing::new(TAIL_BYTES);
    let client_id = format!("agent-run-{}", std::process::id());
    let mut stream = client.attach(session, &client_id, 0, 0, true).await?;
    loop {
        match tokio::time::timeout_at(deadline, stream.frames.recv()).await {
            Err(_) => {
                detach(&stream.control).await;
                return Ok((WaitStatus::Timeout, ring.bytes()));
            }
            Ok(None) => break,
            Ok(Some(frame)) => match frame {
                EventFrame::PaneSnapshot { bytes, .. } | EventFrame::PaneDelta { bytes, .. } => {
                    ring.push(&bytes);
                }
                EventFrame::SessionExit { code, .. } => {
                    detach(&stream.control).await;
                    return Ok((WaitStatus::Exited(code), ring.bytes()));
                }
                _ => {}
            },
        }
    }
    // The stream ended without an exit frame (the session exited between the
    // open and the attach, or the transport dropped): ask the daemon's
    // listing, which keeps tombstones, how it ended.
    let sessions = client.sessions().await?;
    match sessions.iter().find(|s| s.id == session) {
        Some(s) if s.exited_at_ms.is_some() => Ok((WaitStatus::Exited(s.exit_code), ring.bytes())),
        _ => anyhow::bail!(
            "lost the attach stream to session {} before it exited; it may still be running",
            display_cell(session)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_TIMEOUT_SECS, EnvChoice, PIPELINE_ENV, TAIL_BYTES, TAIL_LINES, TailRing,
        WaitStatus, env_choice, output_tail, pin_env_for_run, process_exit_code, strip_ansi,
        tail_lines,
    };
    use thegn_core::config::Config;
    use thegn_core::db::Db;
    use thegn_core::store::WorkspaceStore;
    use thegn_core::testenv::IsolatedState;

    #[test]
    fn env_choice_table() {
        let pin = |n: &str| EnvChoice::Pin(n.to_string());
        // (explicit, has_selection, pipeline_defined) -> choice
        let cases = [
            (Some("host"), false, false, pin("host")),
            (Some("host"), false, true, pin("host")),
            (Some("host"), true, false, pin("host")),
            (Some("host"), true, true, pin("host")),
            (None, true, true, EnvChoice::Keep),
            (None, true, false, EnvChoice::Keep),
            (None, false, true, pin(PIPELINE_ENV)),
            (None, false, false, EnvChoice::KeepNoPipeline),
        ];
        for (explicit, has_selection, pipeline_defined, want) in cases {
            assert_eq!(
                env_choice(explicit, has_selection, pipeline_defined),
                want,
                "{explicit:?} {has_selection} {pipeline_defined}"
            );
        }
        assert_eq!(PIPELINE_ENV, "pipeline");
    }

    fn cfg_with_envs() -> Config {
        toml::from_str(
            "[env.pipeline]\nplacement = \"local\"\n\n[env.host]\nplacement = \"local\"\n",
        )
        .expect("config parses")
    }

    /// A worktree dir (no repo overlay) and its repo root, both real paths.
    fn worktree_fixture(state: &IsolatedState) -> (String, std::path::PathBuf) {
        let root = state.root().join("repo");
        let wt = state.root().join("wt");
        std::fs::create_dir_all(&root).expect("repo dir");
        std::fs::create_dir_all(&wt).expect("wt dir");
        (wt.to_string_lossy().into_owned(), root)
    }

    #[test]
    fn pin_env_pins_an_unbound_worktree_to_pipeline() {
        let state = IsolatedState::new();
        let (wt, root) = worktree_fixture(&state);
        let db = Db::open_memory().expect("db");
        let pinned = pin_env_for_run(&cfg_with_envs(), &db, &wt, &root, None).expect("pin");
        assert_eq!(pinned.as_deref(), Some("pipeline"));
        assert_eq!(
            db.worktree_env(&wt).expect("read").as_deref(),
            Some("pipeline")
        );
    }

    #[test]
    fn pin_env_explicit_env_wins_over_an_existing_selection() {
        let state = IsolatedState::new();
        let (wt, root) = worktree_fixture(&state);
        let db = Db::open_memory().expect("db");
        db.set_worktree_env(&wt, "pipeline").expect("bind");
        let pinned = pin_env_for_run(&cfg_with_envs(), &db, &wt, &root, Some("host")).expect("pin");
        assert_eq!(pinned.as_deref(), Some("host"));
        assert_eq!(db.worktree_env(&wt).expect("read").as_deref(), Some("host"));
    }

    #[test]
    fn pin_env_leaves_a_bound_worktree_untouched() {
        let state = IsolatedState::new();
        let (wt, root) = worktree_fixture(&state);
        let db = Db::open_memory().expect("db");
        db.set_worktree_env(&wt, "host").expect("bind");
        assert_eq!(
            pin_env_for_run(&cfg_with_envs(), &db, &wt, &root, None).expect("keep"),
            None
        );
        assert_eq!(db.worktree_env(&wt).expect("read").as_deref(), Some("host"));
        // A workspace-level selection binds it too.
        let wt2_path = state.root().join("wt2");
        std::fs::create_dir_all(&wt2_path).expect("wt2 dir");
        let wt2 = wt2_path.to_string_lossy().into_owned();
        db.put_workspace(&root.to_string_lossy(), "repo", "repo")
            .expect("workspace");
        db.set_workspace_env(&root.to_string_lossy(), "host")
            .expect("bind workspace");
        assert_eq!(
            pin_env_for_run(&cfg_with_envs(), &db, &wt2, &root, None).expect("keep"),
            None
        );
        assert_eq!(db.worktree_env(&wt2).expect("read"), None);
    }

    #[test]
    fn pin_env_without_a_pipeline_env_keeps_the_default() {
        let state = IsolatedState::new();
        let (wt, root) = worktree_fixture(&state);
        let db = Db::open_memory().expect("db");
        assert_eq!(
            pin_env_for_run(&Config::default(), &db, &wt, &root, None).expect("keep"),
            None
        );
        assert_eq!(db.worktree_env(&wt).expect("read"), None);
    }

    #[test]
    fn pin_env_refuses_an_empty_name_and_a_stale_provider_location() {
        let state = IsolatedState::new();
        let (wt, root) = worktree_fixture(&state);
        let db = Db::open_memory().expect("db");
        assert!(pin_env_for_run(&cfg_with_envs(), &db, &wt, &root, Some("  ")).is_err());
        db.put_worktree("tab", &root.to_string_lossy(), &wt, "feat", None, None)
            .expect("row");
        db.set_worktree_location(
            &wt,
            r#"{"control_prefix":["sprite","exec"],"path":"/workspace"}"#,
        )
        .expect("location");
        let err =
            pin_env_for_run(&cfg_with_envs(), &db, &wt, &root, Some("host")).expect_err("refused");
        assert!(format!("{err:#}").contains("thegn env set"), "{err:#}");
        assert_eq!(db.worktree_env(&wt).expect("read"), None, "no pin written");
        assert_eq!(
            pin_env_for_run(&cfg_with_envs(), &db, &wt, &root, None).expect("keep"),
            None
        );
        assert_eq!(
            db.worktree_env(&wt).expect("read"),
            None,
            "implicit pin skips a provider-bound worktree"
        );
    }

    #[test]
    fn strip_ansi_table() {
        let cases: &[(&[u8], &str)] = &[
            (b"plain text\n", "plain text\n"),
            (b"\x1b[1;31mred\x1b[0m done", "red done"),
            (b"\x1b[?1049h\x1b[2J\x1b[Hscreen", "screen"),
            (b"\x1b]0;window title\x07after", "after"),
            (b"\x1b]8;;https://x.example\x1b\\link\x1b]8;;\x1b\\", "link"),
            (b"\x1bPdcs payload\x1b\\ok", "ok"),
            (b"\x1b(Bcharset", "charset"),
            (b"\x1b7saved\x1b8", "saved"),
            (b"line one\r\nline two\r\n", "line one\nline two\n"),
            (b"progress 10%\rprogress 100%\n", "progress 100%\n"),
            (b"spin\r\x1b[Kdone\n", "done\n"),
            (b"bell\x07 back\x08space", "bell backspace"),
            (b"tab\tkept", "tab\tkept"),
            (
                "caf\u{e9} \u{2014} \u{1f600}\n".as_bytes(),
                "caf\u{e9} \u{2014} \u{1f600}\n",
            ),
            (
                "\x1b[32m\u{65e5}\u{672c}\x1b[0m".as_bytes(),
                "\u{65e5}\u{672c}",
            ),
            ("bidi\u{202e}x".as_bytes(), "bidix"),
            ("c1\u{9b}31m".as_bytes(), "c131m"),
            (b"trailing cr\r", "trailing cr"),
        ];
        for (input, want) in cases {
            assert_eq!(&strip_ansi(input), want, "input {input:?}");
        }
    }

    #[test]
    fn strip_ansi_invalid_utf8_is_replaced_not_dropped() {
        assert_eq!(strip_ansi(b"a\xffb"), "a\u{fffd}b");
    }

    #[test]
    fn tail_ring_is_bounded_and_keeps_the_newest_bytes() {
        let mut ring = TailRing::new(8);
        ring.push(b"abc");
        ring.push(b"defgh");
        assert_eq!(ring.bytes(), b"abcdefgh");
        ring.push(b"ij");
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.bytes(), b"cdefghij");
        ring.push(b"0123456789");
        assert_eq!(ring.bytes(), b"23456789");
        let mut big = TailRing::new(TAIL_BYTES);
        for _ in 0..40 {
            big.push(&[b'x'; 10_000]);
        }
        assert_eq!(big.len(), TAIL_BYTES);
        let mut zero = TailRing::new(0);
        zero.push(b"anything");
        assert_eq!(zero.len(), 0);
    }

    #[test]
    fn tail_lines_caps_and_skips_blank_lines() {
        let text: String = (1..=250).map(|i| format!("line {i}\n\n   \n")).collect();
        let tail = tail_lines(&text, TAIL_LINES);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines.first().copied(), Some("line 51"));
        assert_eq!(lines.last().copied(), Some("line 250"));
        assert_eq!(tail_lines("a  \n\nb\t\n", 5), "a\nb");
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn output_tail_strips_then_caps() {
        let raw = b"\x1b[1mstart\x1b[0m\r\n\r\n\x1b]0;t\x07end\r\n";
        assert_eq!(output_tail(raw), "start\nend");
    }

    #[test]
    fn exit_code_mapping() {
        assert_eq!(process_exit_code(WaitStatus::Exited(Some(0))), 0);
        assert_eq!(process_exit_code(WaitStatus::Exited(Some(1))), 1);
        assert_eq!(process_exit_code(WaitStatus::Exited(Some(137))), 1);
        assert_eq!(process_exit_code(WaitStatus::Exited(Some(-1))), 1);
        assert_eq!(process_exit_code(WaitStatus::Exited(None)), 1);
        assert_eq!(
            process_exit_code(WaitStatus::Timeout),
            crate::cmd::EXIT_RETRYABLE
        );
        assert_eq!(WaitStatus::Timeout.label(), "timeout");
        assert_eq!(WaitStatus::Exited(Some(3)).label(), "exited");
        assert_eq!(WaitStatus::Exited(Some(3)).exit_code(), Some(3));
        assert_eq!(WaitStatus::Timeout.exit_code(), None);
        assert_eq!(DEFAULT_TIMEOUT_SECS, 3600);
    }

    fn parse(args: &[&str]) -> Result<crate::cmd::agent::Action, clap::Error> {
        use clap::Parser as _;
        crate::Cli::try_parse_from(args).map(|cli| match cli.command {
            Some(crate::Command::Agent { action }) => action,
            _ => panic!("expected an agent command"),
        })
    }

    #[test]
    fn agent_run_parses_with_defaults() {
        let action = parse(&[
            "thegn",
            "agent",
            "run",
            "--agent",
            "codex",
            "--worktree",
            "/tmp/wt",
            "--prompt",
            "fix it",
        ])
        .expect("parse");
        let crate::cmd::agent::Action::Run {
            agent,
            worktree,
            env,
            brief_file,
            prompt,
            var,
            track,
            wait,
            timeout,
            json,
        } = action
        else {
            panic!("expected agent run");
        };
        assert_eq!(agent, "codex");
        assert_eq!(worktree, "/tmp/wt");
        assert_eq!(env, None);
        assert_eq!(brief_file, None);
        assert_eq!(prompt.as_deref(), Some("fix it"));
        assert!(var.is_empty());
        assert_eq!(track, None);
        assert!(!wait && !json);
        assert_eq!(timeout, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn agent_run_parses_brief_vars_track_wait() {
        let action = parse(&[
            "thegn",
            "agent",
            "run",
            "--agent",
            "claude",
            "--worktree",
            "wt",
            "--brief-file",
            "brief.md",
            "--var",
            "a=1",
            "--var",
            "b=2",
            "--track",
            "linear:THE-2005",
            "--wait",
            "--timeout",
            "60",
            "--json",
        ])
        .expect("parse");
        let crate::cmd::agent::Action::Run {
            brief_file,
            var,
            track,
            wait,
            timeout,
            json,
            ..
        } = action
        else {
            panic!("expected agent run");
        };
        assert_eq!(brief_file.as_deref(), Some("brief.md"));
        assert_eq!(var, ["a=1", "b=2"]);
        assert_eq!(track.as_deref(), Some("linear:THE-2005"));
        assert!(wait && json);
        assert_eq!(timeout, 60);
    }

    #[test]
    fn agent_run_parses_env() {
        let action = parse(&[
            "thegn",
            "agent",
            "run",
            "--env",
            "x",
            "--prompt",
            "p",
            "--agent",
            "a",
            "--worktree",
            "w",
        ])
        .expect("parse");
        let crate::cmd::agent::Action::Run { env, .. } = action else {
            panic!("expected agent run");
        };
        assert_eq!(env.as_deref(), Some("x"));
    }

    #[test]
    fn agent_run_refuses_bad_flag_combinations() {
        let base = ["thegn", "agent", "run", "--agent", "a", "--worktree", "w"];
        let with = |extra: &[&str]| {
            let mut args = base.to_vec();
            args.extend_from_slice(extra);
            parse(&args).is_err()
        };
        // Exactly one task source.
        assert!(with(&[]), "a task is required");
        assert!(with(&["--prompt", "p", "--brief-file", "f"]));
        // Bindings only render a brief file; a timeout only bounds a wait.
        assert!(with(&["--prompt", "p", "--var", "k=v"]));
        assert!(with(&["--prompt", "p", "--timeout", "5"]));
        // Both identity flags are required.
        assert!(parse(&["thegn", "agent", "run", "--worktree", "w", "--prompt", "p"]).is_err());
        assert!(parse(&["thegn", "agent", "run", "--agent", "a", "--prompt", "p"]).is_err());
    }

    #[test]
    fn agent_run_help_names_the_contract() {
        let mut cmd = <crate::Cli as clap::CommandFactory>::command();
        let run = cmd
            .find_subcommand_mut("agent")
            .and_then(|agent| agent.find_subcommand_mut("run"))
            .expect("agent run subcommand");
        // Whitespace-normalised: clap wraps long help at the terminal width.
        let help = run
            .render_long_help()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for needle in [
            "--agent",
            "--worktree",
            "--brief-file",
            "--prompt",
            "--var",
            "--track",
            "--wait",
            "--timeout",
            "--json",
            "--env",
            "[default: 3600]",
            "left running",
        ] {
            assert!(help.contains(needle), "help lacks {needle}:\n{help}");
        }
    }

    /// `session open`'s plain branch and `agent run` share one open path.
    #[test]
    fn session_open_and_agent_run_share_open_plain() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
        let session = std::fs::read_to_string(src.join("session.rs")).expect("read session.rs");
        let spawn = std::fs::read_to_string(src.join("agent_spawn.rs")).expect("read");
        let arm = session
            .split("SessionAction::Open {\n            agent,")
            .nth(1)
            .and_then(|rest| rest.split("SessionAction::Fork {").next())
            .expect("session open arm");
        assert!(
            arm.contains("open_plain("),
            "session open arm must call open_plain"
        );
        assert!(
            !arm.contains("OpenSpec {"),
            "the plain open spec lives in open_plain only"
        );
        assert!(spawn.contains("session::open_plain("));
    }

    #[test]
    fn result_row_resolution_table() {
        use thegn_core::issue::NewDispatch;
        let _state = IsolatedState::new();
        let db = Db::open_memory().expect("db");
        let err = super::resolve_row_session(&db, 99).unwrap_err().to_string();
        assert_eq!(err, "no dispatch row 99");
        let bare = db
            .put_agent_dispatch(NewDispatch::new("THE-1", "/wt/a", "coder"))
            .expect("row");
        let err = super::resolve_row_session(&db, bare)
            .unwrap_err()
            .to_string();
        assert_eq!(err, format!("dispatch {bare} has no session"));
        let mut new = NewDispatch::new("THE-2", "/wt/b", "coder");
        new.session_id = Some("sess-1");
        let ok = db.put_agent_dispatch(new).expect("row");
        assert_eq!(super::resolve_row_session(&db, ok).expect("ok"), "sess-1");
    }

    #[test]
    fn result_status_mapping() {
        use super::{ResultStatus, status_from_sessions};
        use thegn_svc::control::SessionInfo;
        let live = SessionInfo {
            id: "a".into(),
            ..Default::default()
        };
        let done = SessionInfo {
            id: "b".into(),
            exited_at_ms: Some(5),
            exit_code: Some(7),
            ..Default::default()
        };
        let reaped = SessionInfo {
            id: "c".into(),
            exited_at_ms: Some(5),
            ..Default::default()
        };
        let all = [live, done, reaped];
        assert_eq!(status_from_sessions(&all, "a"), ResultStatus::Running);
        assert_eq!(
            status_from_sessions(&all, "b"),
            ResultStatus::Exited(Some(7))
        );
        assert_eq!(status_from_sessions(&all, "c"), ResultStatus::Exited(None));
        assert_eq!(status_from_sessions(&all, "z"), ResultStatus::Unknown);
    }

    #[test]
    fn result_json_shape_and_exit_codes() {
        use super::{ResultStatus, result_exit_code, result_json};
        let v = result_json(
            Some(4),
            "s",
            ResultStatus::Exited(Some(0)),
            Some("hi"),
            None,
        );
        assert_eq!(
            v,
            serde_json::json!({
                "row": 4, "session": "s", "status": "exited", "exit_code": 0,
                "output_tail": "hi", "tail_note": null
            })
        );
        let v = result_json(None, "s", ResultStatus::Unknown, None, Some("n"));
        assert_eq!(v["status"], "unknown");
        assert!(v["output_tail"].is_null() && v["row"].is_null());
        assert_eq!(result_exit_code(ResultStatus::Exited(Some(0))), 0);
        assert_eq!(result_exit_code(ResultStatus::Exited(Some(2))), 1);
        assert_eq!(result_exit_code(ResultStatus::Exited(None)), 1);
        assert_eq!(result_exit_code(ResultStatus::Running), 3);
        assert_eq!(result_exit_code(ResultStatus::Unknown), 2);
    }

    #[test]
    fn agent_result_parse_matrix() {
        use crate::cmd::agent::Action;
        assert!(parse(&["thegn", "agent", "result"]).is_err());
        assert!(parse(&["thegn", "agent", "result", "--row", "1", "--session", "s"]).is_err());
        assert!(parse(&["thegn", "agent", "result", "--json"]).is_err());
        match parse(&["thegn", "agent", "result", "--row", "7", "--json"]).expect("row") {
            Action::Result { row, session, json } => {
                assert_eq!((row, session, json), (Some(7), None, true));
            }
            _ => panic!("expected agent result"),
        }
        match parse(&["thegn", "agent", "result", "--session", "abc"]).expect("session") {
            Action::Result { row, session, json } => {
                assert_eq!((row, session.as_deref(), json), (None, Some("abc"), false));
            }
            _ => panic!("expected agent result"),
        }
    }
}
