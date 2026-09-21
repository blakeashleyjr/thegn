//! `thegn session …` — drive a *running* pane daemon from the CLI: list its
//! sessions, send terminal input, dump screen snapshots, stream an attach,
//! and inspect relay leases. Every verb is a thin control-API client;
//! with no daemon running they degrade to a clear message (exit 1), never a
//! crash — the spec's "No daemon present" scenario.

use anyhow::{Context, Result};
use base64::Engine as _;
use std::path::Path;
use thegn_core::agent_task::template_vars;
use thegn_core::config::Config;
use thegn_core::db::{Db, ResumeDispatchConflict};
use thegn_core::issue::{AgentDispatchStatus, DispatchRunPublishOutcome, NewDispatch};
use thegn_core::outln;
use thegn_core::pipeline_resume;
use thegn_core::pipeline_run;
use thegn_core::store::{NotificationStore, WorkspaceStore};
use thegn_core::util::git_out;
use thegn_svc::control::client::{AttachControl, ControlAddr, ControlClient};
// NOTE: stage `permissions` ride the daemon's launch command (the harness's
// command-scoped grant, over the *effective* list — THE-440); the dispatch
// only carries the stage name through `AgentLaunch.stage` and refuses, before
// any roster row exists, a grant the harness cannot take (`policy_admission`).

#[derive(clap::Subcommand, Clone)]
pub enum SessionAction {
    /// Move one persisted worktree presentation and dispatch ledger to an
    /// existing profile. This is deliberately a host operation: it crosses
    /// two profile databases and is not a daemon control verb.
    Move {
        /// Exact stored worktree path to migrate.
        worktree: String,
        /// Existing target profile name.
        #[arg(long)]
        to_profile: String,
        /// Kill live source daemon sessions after listing them, before import.
        #[arg(long)]
        kill: bool,
        /// Print the complete migration plan without killing or writing.
        #[arg(long)]
        dry_run: bool,
        /// Emit one redacted audit JSON document instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// List the daemon's sessions — including recently exited ones (the
    /// daemon's tombstones), each marked with a liveness token.
    List {
        /// Emit JSON instead of the human table.
        #[arg(long)]
        json: bool,
        /// Only sessions that have not exited — skip the daemon's tombstones,
        /// so a supervisor polling a fleet never has to re-filter.
        #[arg(long)]
        live: bool,
    },
    /// Open a session running a configured agent, into a worktree — the
    /// headless door onto the daemon's `sessions.open` + `AgentLaunch`
    /// composition (same sandbox/credentials/cap as a TUI launch). Prints the
    /// new session id (THE-57).
    Open {
        /// An `[[agents]]`/`[[tools]]` name, or a provider id (`claude`,
        /// `codex`) when no entry is named that. Required without `--stage`
        /// or `--resume-work`; with `--stage` it defaults to the stage's
        /// configured `agent` — an explicit `--agent` still wins, so a Lead
        /// retrying a stage on a different harness does not have to edit
        /// config. (`--resume-work` always uses the row's recorded agent.)
        #[arg(long, required_unless_present_any = ["stage", "resume_work"])]
        agent: Option<String>,
        /// The worktree to launch into (path). The agent runs here. Not
        /// needed with `--resume-work` — the row's own worktree is the
        /// record.
        #[arg(long, required_unless_present = "resume_work")]
        worktree: Option<String>,
        /// The task to seed the first turn with. Empty ⇒ launch interactively.
        #[arg(long, default_value = "")]
        prompt: String,
        /// Run headlessly (`claude -p …`). Defaults to headless exactly when a
        /// prompt is given.
        #[arg(long)]
        headless: bool,
        /// Record this agent as the worktree's own (`worktrees.agent`), so
        /// resurrection relaunches it and the sidebar attributes its activity.
        #[arg(long)]
        bind: bool,
        /// Ask a running compositor to graft this session into a real pane,
        /// instead of leaving it headless — how a CLI-dispatched agent becomes
        /// something you can watch and type into. A nudge, not a dependency:
        /// granted immediately when the worktree is open in the running
        /// compositor; otherwise the session attaches on its own the moment
        /// that worktree is opened. The session opens either way.
        #[arg(long)]
        adopt: bool,
        /// A `[[pipeline.stages]]` name, with two behaviors. With `--issue`,
        /// the full dispatch: render the stage's prompt template, open the
        /// session headless and write the roster row, in one call — an
        /// explicit `--prompt` is refused (the template owns the task), and a
        /// mistyped stage name is refused offline. Without `--issue`, a plain
        /// open whose launch layers the stage's `model` / `env` /
        /// `permissions` over the agent entry (the stage's overrides; the
        /// agent entry stays the base). Explicit `--agent` wins in both.
        #[arg(long)]
        stage: Option<String>,
        /// Tracker issue id in roster form (`linear:THE-76`). With `--stage`
        /// it dispatches the stage instead of a plain open: it names the
        /// roster row, the artifact directory, and the `{issue_*}` bindings
        /// of the stage's prompt.
        #[arg(long, requires = "stage")]
        issue: Option<String>,
        /// The roster row this one was chunked out of (see `dispatch list`).
        /// Dispatch only (`--stage --issue`).
        #[arg(long, requires = "stage")]
        parent: Option<i64>,
        /// Override the parent's handoff artifact for {parent_artifact}.
        /// Dispatch only (`--stage --issue`).
        #[arg(long, requires = "stage")]
        parent_artifact: Option<String>,
        /// The chunk file this dispatch runs under
        /// (`.thegn/pipeline/<ISSUE>/code/chunk-N.md`), whose `files:`
        /// frontmatter is the row's scope. Dispatch only (`--stage --issue`).
        /// Before the roster row is written, thegn reads it (and every active
        /// sibling's, from each sibling's own worktree) and refuses a scope
        /// collision with an active sibling or an unmet `after:` — the
        /// refusal names the paths and row ids. The explicit override is
        /// `dispatch put --chunk … --force`.
        #[arg(long, requires = "stage", conflicts_with = "resume_work")]
        chunk: Option<String>,
        /// Resume an eligible unfinished pipeline roster row (THE-86): an
        /// unstamped spawning/running row is refused because its worker may
        /// still be live; otherwise the source is reconciled and the fresh
        /// finisher atomically claims the stage's duplicate/capacity gate.
        /// Its stage prompt is rendered against the new finisher row's exact
        /// id/artifact bindings and asks the worker to FINISH the stage —
        /// write and commit that handoff artifact — rather than restart it.
        /// The source row is the record: its
        /// stage, issue, worktree and agent are reused, the new row is
        /// parented on it, and any failure after the insert marks the new row
        /// `failed`.
        /// Conflicts with every flag that would contradict the row
        /// (`--bind`/`--adopt`/`--json` still shape the new session).
        #[arg(
            long,
            conflicts_with_all = [
                "stage",
                "agent",
                "issue",
                "prompt",
                "parent",
                "parent_artifact",
                "worktree",
            ]
        )]
        resume_work: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Fork a live daemon or recorded native harness session into a new
    /// process. A harness id selects a native session from agent sessions.
    Fork {
        /// Live daemon id, or native id when --harness is present.
        session: String,
        /// Harness id for a recorded native session.
        #[arg(long)]
        harness: Option<String>,
        /// Configured agent name for the child launch context.
        #[arg(long)]
        agent: Option<String>,
        /// Child working-directory override.
        #[arg(long)]
        cwd: Option<String>,
        /// Child worktree override.
        #[arg(long)]
        worktree: Option<String>,
        /// Include a bounded plain-text history handoff.
        #[arg(long)]
        scrollback: bool,
        /// Create a new worktree first, then fork into it.
        #[arg(long)]
        fork_worktree: bool,
        /// Adopt the child in a new tab.
        #[arg(long)]
        tab: bool,
        #[arg(long)]
        json: bool,
    },
    /// Close a session: terminate its PTY child (the daemon keeps a tombstone,
    /// so `session list` still shows how it ended and `session wait` still
    /// answers). The dedicated verb for what `thegn api call sessions.kill`
    /// reaches generically — without the `--params '{"s":…}'` foot-gun.
    Close {
        /// Target session id (see `session list`).
        session: String,
        /// Emit `{"session":…,"closed":true}` instead of a human line.
        #[arg(long)]
        json: bool,
    },
    /// Send input to a session's terminal (runs it with `--enter`).
    Send {
        /// Target session id (see `session list`).
        #[arg(long)]
        session: String,
        /// The text to type.
        text: String,
        /// Append a carriage return (send-and-run).
        #[arg(long)]
        enter: bool,
    },
    /// Dump a session's current screen.
    Snapshot {
        #[arg(long)]
        session: String,
        /// Emit JSON (geometry + base64 ANSI) instead of raw screen text.
        #[arg(long)]
        json: bool,
        /// Plain text: the screen rendered to rows of text (no escape
        /// sequences, trailing blanks trimmed) — what an agent or a grep
        /// wants. Combined with --json, adds a `text` field.
        #[arg(long)]
        text: bool,
    },
    /// Stream a session's live output to stdout (Ctrl-C detaches; the
    /// session keeps running).
    Attach {
        #[arg(long)]
        session: String,
    },
    /// Block until a session reaches a state (agent-driving `wait`). Exit 0 on
    /// match, 2 on timeout, 1 if no daemon.
    Wait {
        #[arg(long)]
        session: String,
        /// Condition: `exited` | `idle` | `blocked` | `done` | `match:<regex>`.
        #[arg(long, default_value = "exited")]
        until: String,
        /// Milliseconds before giving up (exit 2). Omit to wait forever.
        #[arg(long)]
        timeout: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Split a session: open a sibling pane (a shell, or the given program).
    Split {
        #[arg(long)]
        session: String,
        /// Placement relative to the target: `right` | `down`.
        #[arg(long, default_value = "right")]
        dir: String,
        /// Program + args for the new pane (defaults to a login shell).
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Record a session's output as an asciicast `.cast` file (server-side;
    /// keeps recording while detached). `--stop` finalizes; with neither it
    /// reports status. The path is printed — the file itself never crosses the
    /// API.
    Record {
        /// Target session id (see `session list`).
        session: String,
        /// Stop and finalize the current recording.
        #[arg(long)]
        stop: bool,
        /// Report status without changing the recording.
        #[arg(long)]
        status: bool,
        /// Emit JSON instead of a human line.
        #[arg(long)]
        json: bool,
    },
    /// Show relay leases (detached sessions kept warm, and until when).
    Leases {
        #[arg(long)]
        json: bool,
    },
}

/// Render one session as the human-table line shared by `session list` and
/// `thegn attach` (no-arg listing).
///
/// The liveness token sits immediately after the id so a supervisor can grep a
/// fixed column: `live`, or `exited(<code>)` — `exited(?)` when the child could
/// not be reaped — suffixed with the `final_state` word when the daemon has
/// one, e.g. `exited(0,done)`. The wire has carried this data all along
/// (`SessionInfo::exited_at_ms`); printing it is what makes the listing
/// truthful about a worker that already finished.
pub(crate) fn session_line(s: &thegn_svc::control::SessionInfo) -> String {
    let state = match s.exited_at_ms {
        None => "live".to_string(),
        Some(_) => {
            let code = s
                .exit_code
                .map_or_else(|| "?".to_string(), |c| c.to_string());
            match s.final_state.as_deref() {
                Some(w) if !w.is_empty() => format!("exited({code},{w})"),
                _ => format!("exited({code})"),
            }
        }
    };
    let lease = s
        .lease_expires_at
        .map(|at| format!("  lease→{at}"))
        .unwrap_or_default();
    let lineage = s
        .forked_from
        .as_deref()
        .map(|source| format!("  ← forked from {source}"))
        .unwrap_or_default();
    format!(
        "{}  {}  {}x{}  {} client(s)  {}{}{}{}",
        s.id,
        state,
        s.cols,
        s.rows,
        s.attached_clients,
        s.program,
        s.worktree
            .as_deref()
            .map(|w| format!("  [{w}]"))
            .unwrap_or_default(),
        lease,
        lineage
    )
}

/// Parse a `--until`/`condition` string into a `WaitCondition` JSON value.
/// `match:<rx>` waits on an output regex; the bare words map to the named
/// conditions. Shared with the MCP `sessions_wait` tool (`cmd::mcp`) so the
/// mini-grammar has exactly one implementation.
pub(crate) fn parse_wait_condition(s: &str) -> Result<serde_json::Value> {
    let v = match s {
        "exited" => serde_json::json!({ "kind": "exited" }),
        "idle" => serde_json::json!({ "kind": "idle" }),
        "blocked" => serde_json::json!({ "kind": "blocked" }),
        "done" => serde_json::json!({ "kind": "done" }),
        other => match other.strip_prefix("match:") {
            Some(rx) => serde_json::json!({ "kind": "output_matches", "regex": rx }),
            None => anyhow::bail!(
                "unknown wait condition '{other}' \
                 (expected exited|idle|blocked|done|match:<regex>)"
            ),
        },
    };
    Ok(v)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
        .unwrap_or(0)
}

/// Discover the local daemon (registry first, configured socket as fallback)
/// and verify it answers. `Err` carries the user-facing no-daemon message.
/// Shared with `thegn attach` (`cmd::attach`), the local interactive client.
pub(crate) async fn connect(cfg: &Config) -> Result<ControlClient> {
    let addr = Db::open()
        .ok()
        .and_then(|db| {
            thegn_svc::control::client::discover(&db, &crate::daemon::scope_key(), now_ms())
        })
        .unwrap_or_else(|| ControlAddr::Unix(crate::daemon::socket_path(&cfg.daemon)));
    let client = ControlClient::new(addr);
    if client.health().await.is_err() {
        anyhow::bail!(
            "no thegn pane daemon is running — start one with `thegn serve`, \
             or enable `[daemon]` in config so the compositor keeps one warm"
        );
    }
    Ok(client)
}

/// Run a session verb. Exit-code semantics: daemon absent ⇒ 1 with the clear
/// message above (`--json` verbs emit `{"error":"no_daemon"}` on stdout).
pub fn run(cfg: &Config, action: SessionAction) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cfg, action))
}

async fn run_async(cfg: &Config, action: SessionAction) -> Result<()> {
    let json_mode = matches!(
        &action,
        SessionAction::Move { json: true, .. }
            | SessionAction::List { json: true, .. }
            | SessionAction::Open { json: true, .. }
            | SessionAction::Fork { json: true, .. }
            | SessionAction::Close { json: true, .. }
            | SessionAction::Snapshot { json: true, .. }
            | SessionAction::Wait { json: true, .. }
            | SessionAction::Record { json: true, .. }
            | SessionAction::Leases { json: true }
    );
    // `session open`'s offline refusals run before `connect`: all are caller
    // mistakes answerable without a daemon (the smoke suite checks them
    // daemon-free) — see `open_preflight`. `--resume-work` carries its own
    // offline row checks (unknown row, non-pipeline row, unknown stage) in
    // `resume_preflight`, the same pre-`connect` slot.
    match &action {
        SessionAction::Move { .. } => {
            // Migration is dispatched before `connect`: a cold source daemon
            // is a supported case, and the target is never loaded in-process.
            return crate::cmd::session_move::run(cfg, action).await;
        }
        SessionAction::Open {
            stage,
            issue,
            prompt,
            headless,
            resume_work: None,
            ..
        } => {
            open_preflight(cfg, stage.as_deref(), issue.as_deref(), prompt, *headless)?;
        }
        SessionAction::Open {
            resume_work: Some(row_id),
            ..
        } => {
            let db = Db::open()?;
            resume_preflight(cfg, &db, *row_id)?;
        }
        _ => {}
    }
    let client = match connect(cfg).await {
        Ok(c) => c,
        Err(e) => {
            if json_mode {
                outln!("{}", serde_json::json!({ "error": "no_daemon" }));
            }
            return Err(e);
        }
    };
    match action {
        SessionAction::Move { .. } => unreachable!("session move was dispatched before connect"),
        SessionAction::List { json, live } => {
            let mut sessions = client.sessions().await?;
            if live {
                // The daemon lists tombstones too (recently exited sessions,
                // still readable); `--live` keeps only sessions that have not
                // exited, in both human and JSON mode.
                sessions.retain(|s| s.exited_at_ms.is_none());
            }
            if json {
                outln!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                outln!("no live sessions");
            } else {
                for s in &sessions {
                    outln!("{}", session_line(s));
                }
            }
        }
        SessionAction::Open {
            agent,
            worktree,
            prompt,
            headless,
            bind,
            adopt,
            stage,
            issue,
            parent,
            parent_artifact,
            chunk,
            resume_work: resume_row,
            json,
        } => {
            use thegn_svc::control::{AgentLaunch, OpenSpec};
            if let Some(row_id) = resume_row {
                // The resume-of-a-failed-row composition (THE-86): the
                // offline row checks already ran pre-`connect`; this renders
                // the finisher prompt and opens the fresh dispatch.
                // `--bind`/`--adopt`/`--json` shape the new session exactly
                // as they shape a fresh one.
                resume_work(cfg, &client, row_id, bind, adopt, json).await?;
            } else if let (Some(stage_name), Some(issue_id)) = (stage.as_deref(), issue.as_deref())
            {
                // The one-call stage dispatch (THE-76): the whole
                // render → open → roster composition in `open_stage`.
                // `--agent` is optional here (the stage's configured agent is
                // the default); the prompt comes from the stage's template,
                // so an explicit `--prompt` is refused in `open_preflight`.
                open_stage(
                    cfg,
                    &client,
                    StageDispatch {
                        stage: stage_name,
                        issue: issue_id,
                        parent,
                        parent_artifact: parent_artifact.as_deref(),
                        chunk: chunk.as_deref(),
                        agent: agent.as_deref(),
                        // clap requires --worktree unless --resume-work (and
                        // the two conflict), so this is always Some here.
                        worktree: worktree
                            .as_deref()
                            .expect("clap requires --worktree without --resume-work"),
                        bind,
                        adopt,
                        json,
                    },
                )
                .await?;
            } else {
                // A plain open (THE-57) — optionally with a stage layered over
                // the agent: `--stage` without `--issue` applies that stage's
                // `model`/`env`/`permissions` (THE-83) and defaults the agent
                // to the stage's configured one. A promptless open stays an
                // interactive launch.
                let agent = match agent {
                    Some(a) => a,
                    None => {
                        let name = stage
                            .as_deref()
                            .expect("clap requires --agent without --stage");
                        cfg.pipeline
                            .stage(name)
                            .map(|s| s.agent.clone())
                            .expect("open_preflight checked the stage exists")
                    }
                };
                // Resolve the worktree to an absolute path (the agent launches
                // here; the daemon resolves the sandbox/env from it).
                let wt = crate::cmd::resolve_worktree(worktree)
                    .to_string_lossy()
                    .into_owned();
                let spec = OpenSpec {
                    automation_origin: None,
                    argv: Vec::new(),
                    cwd: None,
                    env: Vec::new(),
                    rows: 24,
                    cols: 80,
                    worktree: Some(wt),
                    agent: Some(AgentLaunch {
                        agent,
                        prompt,
                        // A plain `--headless` forces headless; absent leaves the
                        // default (headless exactly when a prompt was given).
                        headless: headless.then_some(true),
                        bind_worktree: bind,
                        // No `--resume` on this CLI path: launch cold.
                        resume: None,
                        // A fresh launch never continues a dead session.
                        continue_last: false,
                        // `--stage` without `--issue` layers the stage's
                        // model/env/permissions over the agent; None on a
                        // plain open.
                        stage,
                        fork: false,
                        native_session_id: None,
                    }),
                    // Default false: a fan-out that spawns eight agents should not
                    // yank eight panes into the user's session unasked. `--adopt`
                    // is the opt-in the pipeline skill always passes.
                    adopt,
                    already_capped: false,
                };
                let info = client.open(&spec).await?;
                if json {
                    outln!("{}", serde_json::to_string_pretty(&info)?);
                } else {
                    outln!("{}", info.id);
                }
            }
        }
        SessionAction::Fork {
            session,
            harness,
            agent,
            cwd,
            worktree,
            scrollback,
            fork_worktree,
            tab,
            json,
        } => {
            crate::cmd::session_fork::run(
                &client,
                session,
                harness,
                agent,
                cwd,
                worktree,
                scrollback,
                fork_worktree,
                tab,
                json,
            )
            .await?;
        }
        SessionAction::Close { session, json } => {
            client.kill(&session).await?;
            if json {
                outln!(
                    "{}",
                    serde_json::json!({ "session": session, "closed": true })
                );
            } else {
                outln!("closed {session}");
            }
        }
        SessionAction::Send {
            session,
            text,
            enter,
        } => {
            client.send_input(&session, text.as_bytes(), enter).await?;
            outln!(
                "sent {} byte(s) to {session}",
                text.len() + usize::from(enter)
            );
        }
        SessionAction::Snapshot {
            session,
            json,
            text,
        } => {
            let (seq, rows, cols, ansi) = client.snapshot(&session).await?;
            // The wire carries the screen as an ANSI repaint; render it back
            // to plain rows through the same emulator the panes use.
            let plain = text.then(|| snapshot_text(rows, cols, &ansi));
            if json {
                let mut doc = serde_json::json!({
                    "session": session, "seq": seq, "rows": rows, "cols": cols,
                    "ansi_b64": base64::engine::general_purpose::STANDARD.encode(&ansi),
                });
                if let Some(t) = plain {
                    doc["text"] = serde_json::Value::String(t);
                }
                outln!("{doc}");
            } else if let Some(t) = plain {
                outln!("{t}");
            } else {
                // Raw ANSI to stdout: piping into a terminal repaints the
                // screen; piping into a file keeps the escape stream.
                use std::io::Write;
                std::io::stdout().write_all(&ansi)?;
                std::io::stdout().flush()?;
            }
        }
        SessionAction::Attach { session } => {
            let client_id = format!("cli-{}", std::process::id());
            let mut stream = client.attach(&session, &client_id, 0, 0, true).await?;
            // Observer stream: snapshot then deltas, raw to stdout. Ctrl-C
            // (SIGINT) ends the process; the daemon prunes the subscriber and
            // the session lives on (that's the point).
            use std::io::Write;
            let mut out = std::io::stdout();
            while let Some(frame) = stream.frames.recv().await {
                use thegn_core::control_wire::EventFrame;
                match frame {
                    EventFrame::PaneSnapshot { bytes, .. }
                    | EventFrame::PaneDelta { bytes, .. } => {
                        out.write_all(&bytes)?;
                        out.flush()?;
                    }
                    EventFrame::SessionExit { code, .. } => {
                        let _ = stream.control.send(AttachControl::Close).await; // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                        outln!(
                            "\n[session exited: {}]",
                            code.map_or("?".into(), |c| c.to_string())
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
        SessionAction::Wait {
            session,
            until,
            timeout,
            json,
        } => {
            let condition = parse_wait_condition(&until)?;
            let outcome = client.wait(&session, condition, timeout).await?;
            let matched = outcome
                .get("matched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if json {
                outln!("{}", serde_json::to_string_pretty(&outcome)?);
            } else if matched {
                let what = outcome
                    .get("condition")
                    .and_then(|c| c.as_str())
                    .unwrap_or("condition");
                let code = outcome
                    .get("exit_code")
                    .and_then(|c| c.as_i64())
                    .map(|c| format!(" (exit {c})"))
                    .unwrap_or_default();
                outln!("{what} on {session}{code}");
            } else {
                outln!("timeout waiting on {session}");
            }
            if !matched {
                std::process::exit(crate::cmd::EXIT_RETRYABLE);
            }
        }
        SessionAction::Split { session, dir, argv } => {
            let info = client.split(&session, &dir, &argv).await?;
            outln!("opened {} ({}x{})", info.id, info.cols, info.rows);
        }
        SessionAction::Record {
            session,
            stop,
            status,
            json,
        } => {
            let op = if stop {
                "stop"
            } else if status {
                "status"
            } else {
                "start"
            };
            let st = client.record(&session, op).await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                let where_ = st.path.as_deref().unwrap_or("(no file)");
                if let Some(reason) = &st.truncated {
                    // Never report a truncated recording as saved: the file is
                    // short of the session's last output.
                    outln!("recording TRUNCATED → {where_} ({reason})");
                } else if st.recording {
                    outln!("recording {session} → {where_} ({} bytes)", st.bytes);
                } else if st.capped {
                    outln!("recording stopped at size cap → {where_}");
                } else {
                    outln!(
                        "not recording{}",
                        st.path.map(|p| format!(" (last: {p})")).unwrap_or_default()
                    );
                }
            }
        }
        SessionAction::Leases { json } => {
            let v = client.leases().await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let leases = v
                    .get("leases")
                    .and_then(|l| l.as_array())
                    .cloned()
                    .unwrap_or_default();
                if leases.is_empty() {
                    outln!("no leases");
                }
                for l in leases {
                    outln!(
                        "{}  {}  expires_at={}",
                        l.get("session").and_then(|s| s.as_str()).unwrap_or("?"),
                        l.get("kind").and_then(|s| s.as_str()).unwrap_or("?"),
                        l.get("expires_at")
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "-".into()),
                    );
                }
            }
        }
    }
    Ok(())
}

/// The parsed `session open --stage` invocation, ready to dispatch. A struct
/// rather than an eleven-argument call — same reason `NewDispatch` exists.
struct StageDispatch<'a> {
    stage: &'a str,
    issue: &'a str,
    parent: Option<i64>,
    parent_artifact: Option<&'a str>,
    /// The chunk file this dispatch runs under (the scope gate reads it
    /// before the insert; the row records the path).
    chunk: Option<&'a str>,
    /// Explicit `--agent`, overriding the stage's configured one.
    agent: Option<&'a str>,
    worktree: &'a str,
    bind: bool,
    adopt: bool,
    json: bool,
}

/// The tracker facts a stage prompt may reference live in
/// [`crate::stage_prompt`] — moved verbatim (THE-86 chunk 2) so the daemon's
/// transport-retry relaunch renders identically to this dispatch path.
use crate::stage_prompt::{IssueFacts, stage_task_vars};

/// Step 1 of the stage dispatch: resolve a stage by name, listing what IS
/// configured on a miss. Called from `open_preflight` (before `connect`, so a
/// config typo is answerable without a daemon) and again in `open_stage`, so
/// the message lives in exactly one place.
fn stage_or_bail<'a>(
    cfg: &'a Config,
    name: &str,
) -> Result<&'a thegn_core::config_pipeline::PipelineStage> {
    cfg.pipeline.stage(name).ok_or_else(|| {
        let names = cfg.pipeline.stage_names();
        if names.is_empty() {
            anyhow::anyhow!("no [[pipeline.stages]] named '{name}' — no stages are configured")
        } else {
            anyhow::anyhow!(
                "no [[pipeline.stages]] named '{name}' — configured stages: {}",
                names.join(", ")
            )
        }
    })
}

/// Admit a stage launch's effective permission policy before anything is
/// claimed or spawned (THE-440). The grant must be expressible as the
/// harness's command-scoped mechanism; otherwise the dispatch is held with an
/// actionable error — no roster row, no process, no task failure recorded.
/// The daemon re-derives the same command from the same config, so this is
/// an early, row-free refusal, not a second source of truth.
///
/// An agent/stage that does not resolve at all is left to the daemon's launch
/// (which fails the row exactly as before); only the policy is admitted here.
pub(crate) fn policy_admission(cfg: &Config, agent: &str, stage: &str) -> Result<()> {
    let Ok(eff) = thegn_core::agent_task::effective_agent(cfg, agent, Some(stage)) else {
        return Ok(());
    };
    eff.permission_args()
        .map(drop)
        .map_err(|why| anyhow::anyhow!("stage `{stage}` (agent `{agent}`) not dispatched: {why}"))
}

/// The `session open` refusals that are answerable before `connect` — both are
/// caller mistakes, not daemon problems:
///
/// - `--stage` names a stage that does not exist (step 1 of the dispatch);
/// - explicit `--headless` with a blank prompt would launch an agent that
///   blocks on stdin forever. (A plain open with no prompt stays an
///   interactive launch — a real and correct use. A *stage's* rendered prompt
///   is refused in `open_stage`, where the roster row exists and can be
///   marked `failed`.)
///
/// `--resume-work` runs its own offline row checks in this same pre-`connect`
/// slot — `resume_preflight`: unknown row, non-pipeline row, unknown stage —
/// so its refusals are answerable without a daemon either.
fn open_preflight(
    cfg: &Config,
    stage: Option<&str>,
    issue: Option<&str>,
    prompt: &str,
    headless: bool,
) -> Result<()> {
    if let Some(name) = stage {
        stage_or_bail(cfg, name)?;
    }
    if issue.is_some() {
        // Dispatch: the stage's template owns the task. An explicit --prompt
        // would be silently ignored, so it is refused — a caller mistake,
        // answerable offline.
        if !prompt.trim().is_empty() {
            anyhow::bail!(
                "--prompt is refused with a --stage dispatch: the stage's \n                 prompt template owns the task"
            );
        }
    } else if headless && prompt.trim().is_empty() {
        anyhow::bail!(
            "--headless with an empty prompt would launch an agent that blocks \
             on stdin — give --prompt or drop --headless"
        );
    }
    Ok(())
}

// `stage_task_vars` (the nine `agent_task::STAGE_VARS` for one stage
// dispatch) lives in [`crate::stage_prompt`] (moved verbatim, THE-86 chunk
// 2) — re-imported above so every use site here is unchanged.

/// The tracker facts a stage prompt may reference — the factored
/// `template_vars`/`needs_tracker` block, so the two render paths (a fresh
/// `--stage --issue` dispatch and a `--resume-work` finisher) read the
/// tracker through one implementation.
async fn gather_issue_facts(
    client: &ControlClient,
    stage: &thegn_core::config_pipeline::PipelineStage,
    issue_id: &str,
) -> Result<IssueFacts> {
    let referenced = template_vars(&stage.prompt)
        .map_err(|e| anyhow::anyhow!("stage '{}' prompt template is invalid: {e}", stage.name))?;
    let needs_tracker = ["issue_title", "issue_body", "issue_url"]
        .iter()
        .any(|v| referenced.iter().any(|r| r == v));
    Ok(if needs_tracker {
        let detail = client.issue_get(issue_id).await?;
        let issue = detail.issue;
        IssueFacts {
            number: pipeline_run::issue_key(issue_id),
            title: issue.title,
            body: issue.body.unwrap_or_default(),
            url: issue.url,
        }
    } else {
        IssueFacts {
            number: pipeline_run::issue_key(issue_id),
            title: String::new(),
            body: String::new(),
            url: String::new(),
        }
    })
}

/// The branch a worktree is on: the registered worktree row's branch, else
/// `git rev-parse --abbrev-ref HEAD` — the same two-tier lookup a
/// daemon-launched agent uses. Empty is acceptable (a detached or unborn
/// HEAD). Factored out of `open_stage` so `--resume-work` resolves the
/// branch identically.
fn resolve_branch(db: &Db, wt: &str) -> String {
    let registered = db
        .worktrees()
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|r| r.worktree == wt)
                .map(|r| r.branch)
        })
        .filter(|b| !b.is_empty());
    registered.unwrap_or_else(|| {
        git_out(Path::new(wt), &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default()
    })
}

/// The `--stage` dispatch: the Lead's hand-rolled loop, performed in one call
/// (design §3 item 3). The order of operations is deliberate — the roster row
/// goes in **before** the session opens (D5: a crash between the two leaves a
/// visible `queued` row, re-drivable, instead of a live agent nobody has a
/// record of), and everything after the insert leaves the row `failed`, never
/// stuck `queued`, on the way out.
///
/// The stage's `model` / `env` / `permissions` are NOT applied here: the
/// dispatch carries the stage name through `AgentLaunch.stage`, so the daemon
/// resolves the effective agent exactly as a TUI launch does (`command_for`
/// layers the stage over the entry and appends the effective allow-list as the
/// harness's command-scoped grant). The dispatch only ADMITS the policy
/// ([`policy_admission`]) before the roster row is claimed, so an
/// unattestable grant is a pre-launch hold, never a failed task.
async fn open_stage(cfg: &Config, client: &ControlClient, d: StageDispatch<'_>) -> Result<()> {
    use thegn_svc::control::{AgentLaunch, OpenSpec};

    // 1. The stage lookup (also done in `open_preflight`, before `connect`).
    let stage = stage_or_bail(cfg, d.stage)?;
    // 2. Absolute worktree path — the agent launches here.
    let wt = crate::cmd::resolve_worktree(Some(d.worktree.to_string()))
        .to_string_lossy()
        .into_owned();
    let db = Db::open()?;
    // 3. Branch: the registered worktree row, else `git rev-parse` — the same
    //    two-tier lookup a daemon-launched agent uses. Empty is acceptable.
    let branch = resolve_branch(&db, &wt);
    // 4. Issue facts. The tracker is consulted only when the prompt reads it:
    //    `{issue_number}` is local (the id with its provider prefix stripped),
    //    the other three need the daemon's tracker door. When the template
    //    references one of them and the lookup fails, the tracker's own error
    //    propagates — a prompt with a silently empty issue body is how a
    //    worker ends up implementing nothing.
    let facts = gather_issue_facts(client, stage, d.issue).await?;
    // 5. A parent must exist — the same rule `dispatch put` enforces; restated
    //    here (rather than imported) so the two verbs stay uncoupled. The row
    //    is kept: its own `artifact_path` is `{parent_artifact}`'s default.
    let parent_row = match d.parent {
        Some(id) => Some(
            db.get_dispatch(id)?
                .ok_or_else(|| anyhow::anyhow!("no dispatch with id {id} to parent this row on"))?,
        ),
        None => None,
    };
    // 6. The chunk-scope gate BEFORE the insert — a refused dispatch must
    //    leave no row behind. Same helper `dispatch put --chunk` uses (two
    //    callers, one refusal); no --force here: an intentional overlap is
    //    declared in the chunk's `overlaps:` frontmatter, and the explicit
    //    override lives on `dispatch put`.
    if let Some(chunk_path) = d.chunk {
        super::dispatch::chunk_gate(&db, &wt, d.issue, chunk_path, false)?;
    }
    // 6b. Insert the roster row BEFORE opening the session (D5).
    let agent_name = d
        .agent
        .map(str::to_string)
        .unwrap_or_else(|| stage.agent.clone());
    // 6a. Permission-policy admission, BEFORE the claim: a grant the harness
    //     cannot take command-scoped leaves no row and spawns nothing.
    policy_admission(cfg, &agent_name, &stage.name)?;
    // Route the insert through the atomic claim rather than a bare append.
    // This is the path a Lead actually dispatches on, so it is the one that
    // must not race: `dispatch list` -> judgment -> insert is a
    // read-modify-write two monitors (or one monitor and its own restart) can
    // interleave, and counting live sessions instead of rows is what let 33
    // issues run against a budget of 9 on 2026-08-29.
    //
    // A closed row frees its slot, so re-dispatching a stage after reconciling
    // the previous attempt is unaffected — only rows still occupying a slot
    // refuse. The deliberate-duplicate escape hatch lives on `dispatch claim
    // --allow-duplicate <reason>`, which records the justification.
    let row_id = match db.claim_dispatch(
        NewDispatch {
            issue_id: d.issue,
            worktree_path: &wt,
            agent_name: &agent_name,
            stage: Some(&stage.name),
            parent_id: d.parent,
            session_id: None,
            artifact_path: None,
            chunk_path: d.chunk,
        },
        stage.concurrency,
        None,
    )? {
        Ok(id) => id,
        Err(decision) => {
            let reason = decision.reason();
            if d.json {
                super::emit_json(&serde_json::json!({
                    "granted": false,
                    "reason": reason.as_str(),
                }))?;
            }
            return Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                "stage dispatch refused: {reason}\n(override with `thegn dispatch claim … \
                 --allow-duplicate <reason>` if this really is separate work)"
            ))));
        }
    };
    // 7. The artifact path this stage's worker will write (D6: sanitized,
    //    per-issue, row-keyed — the row id keeps parallel coders collide-free).
    let artifact = pipeline_run::artifact_path(d.issue, &stage.name, row_id);
    // 8–11. Render, refuse an empty prompt, open. Any failure from here on
    //    must leave the row `failed`: a row stuck `queued` reads as "not yet
    //    driven", and the Lead would re-drive it forever.
    let parent_artifact = d
        .parent_artifact
        .map(str::to_string)
        .or_else(|| parent_row.as_ref().and_then(|r| r.artifact_path.clone()))
        .unwrap_or_default();
    let opened = async {
        let vars = stage_task_vars(
            &facts,
            &branch,
            &wt,
            &stage.name,
            &artifact,
            &parent_artifact,
            row_id,
        );
        let prompt = crate::stage_prompt::render_stage(&stage.name, &stage.prompt, &vars)?;
        let spec = OpenSpec {
            automation_origin: None,
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
            worktree: Some(wt.clone()),
            agent: Some(AgentLaunch {
                agent: agent_name,
                prompt,
                // A stage dispatch is always headless — an interactive
                // worker is a pane that sits there forever.
                headless: Some(true),
                bind_worktree: d.bind,
                resume: None,
                // A stage dispatch opens a fresh session, never a continue.
                continue_last: false,
                // The stage name rides along: the daemon layers the stage's
                // `model`/`env`/`permissions` over the resolved agent and
                // seeds the effective allow-list (THE-83's launch path).
                stage: Some(stage.name.clone()),
                fork: false,
                native_session_id: None,
            }),
            adopt: d.adopt,
            already_capped: false,
        };
        client.open(&spec).await
    }
    .await;
    match opened {
        Ok(info) => {
            // 12. Publish session + artifact + `running` in one DB write. If
            // that fails after the daemon opened the process, compensate by
            // killing the process and closing the visible row as failed.
            publish_opened_dispatch(&db, client, row_id, &info.id, &artifact).await?;
            // 13. Print.
            if d.json {
                outln!(
                    "{}",
                    serde_json::json!({
                        "row": row_id,
                        "session": info.id,
                        "stage": stage.name,
                        "artifact": artifact,
                        "issue": d.issue,
                        "worktree": wt,
                        "branch": branch,
                    })
                );
            } else {
                outln!(
                    "dispatch {row_id} → session {} (stage {})",
                    info.id,
                    stage.name
                );
                outln!("artifact {artifact} (branch {branch})");
            }
            Ok(())
        }
        Err(e) => {
            // best-effort: the original error is what the operator needs; if
            // even the failed-stamp cannot land (the roster is a cache), the
            // row stays visibly wrong either way and the wrapped error still
            // names it.
            let _ = db.update_dispatch_status(row_id, AgentDispatchStatus::Failed); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            Err(e).with_context(|| format!("dispatch {row_id} failed"))
        }
    }
}

/// Publish an already-opened daemon session on its roster row. The daemon and
/// SQLite cannot share one transaction, so every missing/stale/error outcome is
/// compensated immediately by killing the new session. A concurrent roster
/// verdict is preserved; an otherwise-owned queued/spawning row is failed
/// best-effort. Cleanup failures are included in the returned error.
async fn publish_opened_dispatch(
    db: &Db,
    client: &ControlClient,
    row_id: i64,
    session_id: &str,
    artifact: &str,
) -> Result<()> {
    let (publish_detail, reconcile_owned_row) =
        match db.publish_dispatch_run(row_id, session_id, artifact) {
            Ok(DispatchRunPublishOutcome::Published) => return Ok(()),
            Ok(DispatchRunPublishOutcome::Missing) => {
                ("the reserved roster row disappeared".to_string(), false)
            }
            Ok(DispatchRunPublishOutcome::StateChanged { status }) => (
                format!(
                    "the roster row moved to {} while the session was opening",
                    status.as_str()
                ),
                false,
            ),
            Err(error) => (format!("the roster publication failed: {error:#}"), true),
        };
    let kill = client.kill(session_id).await;
    let row_detail = if reconcile_owned_row {
        let note = if kill.is_ok() {
            "session opened but its roster identity could not be published; launch was torn down"
        } else {
            "session opened but its roster identity could not be published; launch teardown failed"
        };
        let mut result: Result<bool> = Ok(false);
        for expected in [AgentDispatchStatus::Queued, AgentDispatchStatus::Spawning] {
            match db.compare_and_set_dispatch_status(
                row_id,
                expected,
                AgentDispatchStatus::Failed,
                Some(note),
            ) {
                Ok(true) => {
                    result = Ok(true);
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        match result {
            Ok(true) => "reserved row marked failed".to_string(),
            Ok(false) => "reserved row changed concurrently and was preserved".to_string(),
            Err(error) => format!("row reconciliation failed: {error:#}"),
        }
    } else {
        "newer roster state preserved".to_string()
    };
    let kill_detail = match kill {
        Ok(()) => "session teardown succeeded".to_string(),
        Err(error) => format!("session teardown failed: {error:#}"),
    };
    anyhow::bail!(
        "dispatch {row_id} opened session {session_id} but could not publish its roster identity: \
         {publish_detail}; {kill_detail}; {row_detail}"
    )
}

/// The row checks `--resume-work` answers offline, before any daemon contact
/// (the smoke suite checks them daemon-free): the row must exist — same
/// wording `dispatch set-status` uses — and must be a pipeline row, because a
/// plain dispatch has no stage to re-render, so there is nothing to resume.
/// Pure over the fetched row so the wording is testable without a client;
/// returns the stage name to resume.
fn resume_row_checks(row_id: i64, row: Option<&thegn_core::issue::AgentDispatch>) -> Result<&str> {
    let row = row.ok_or_else(|| anyhow::anyhow!("no dispatch with id {row_id}"))?;
    // A finished or retired verdict is not a resume point: the Lead already
    // closed the stage (done/merged) or walked away from it (abandoned), and
    // re-driving it would spawn a second worker over closed work. `failed` —
    // the resume feature's whole point — and eligible parked/exited states
    // stay resumable after their source row is reconciled deliberately.
    if matches!(
        row.status,
        AgentDispatchStatus::Done | AgentDispatchStatus::Merged | AgentDispatchStatus::Abandoned
    ) {
        anyhow::bail!(
            "dispatch {row_id} is {} — a finished or retired verdict is not a resume \
             point; --resume-work re-drives a failed or parked row",
            row.status.as_str()
        );
    }
    if matches!(
        row.status,
        AgentDispatchStatus::Spawning | AgentDispatchStatus::Running
    ) && row.exit_code.is_none()
        && row.exited_at_ms.is_none()
    {
        anyhow::bail!(
            "dispatch {row_id} is {} with no exit stamp — its worker may still be live; wait for \
             its exit or close it deliberately before --resume-work",
            row.status.as_str()
        );
    }
    if row.status == AgentDispatchStatus::Unknown {
        anyhow::bail!(
            "dispatch {row_id} has an unknown status — reconcile the roster row before --resume-work"
        );
    }
    row.stage
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "dispatch {row_id} is not a pipeline row (no stage) — \
                 --resume-work resumes a --stage dispatch"
            )
        })
}

/// `--resume-work`'s offline refusals, run in the same pre-`connect` slot as
/// [`open_preflight`]: row lookup, pipeline-row check, stage existence. A
/// caller mistake here is answerable from the local roster and config alone —
/// never a daemon error.
fn resume_preflight(cfg: &Config, db: &Db, row_id: i64) -> Result<()> {
    let row = db.get_dispatch(row_id)?;
    let stage_name = resume_row_checks(row_id, row.as_ref())?;
    stage_or_bail(cfg, stage_name)?;
    Ok(())
}

/// Reconcile one eligible source row and atomically admit its finisher. Kept
/// separate from the daemon open so capacity/de-dup behavior is testable with
/// an in-memory roster and no process.
fn claim_resume_dispatch(
    db: &Db,
    row: &thegn_core::issue::AgentDispatch,
    stage: &thegn_core::config_pipeline::PipelineStage,
    worktree: &str,
    agent_name: &str,
    json: bool,
) -> Result<i64> {
    let claim = match db.claim_resume_dispatch(
        row.id,
        row.status,
        NewDispatch {
            issue_id: &row.issue_id,
            worktree_path: worktree,
            agent_name,
            stage: row.stage.as_deref(),
            parent_id: Some(row.id),
            session_id: None,
            artifact_path: None,
            chunk_path: row.chunk_path.as_deref(),
        },
        stage.concurrency,
    ) {
        Ok(claim) => claim,
        Err(error) => {
            // Only the typed lost-verdict race is retryable. Other DB errors
            // retain the normal fatal exit code rather than being mistaken for
            // contention.
            if resume_claim_is_conflict(&error) {
                if json {
                    super::emit_json(&resume_conflict_payload())?;
                }
                return Err(anyhow::Error::new(crate::cmd::Retryable(error)));
            }
            return Err(error);
        }
    };
    match claim {
        Ok(id) => Ok(id),
        Err(decision) => {
            let reason = decision.reason();
            if json {
                super::emit_json(&serde_json::json!({
                    "granted": false,
                    "reason": reason.as_str(),
                }))?;
            }
            Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
                "resume dispatch refused; row {} was left unchanged: {reason}",
                row.id
            ))))
        }
    }
}

fn resume_claim_is_conflict(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ResumeDispatchConflict>().is_some()
}

fn resume_conflict_payload() -> serde_json::Value {
    serde_json::json!({
        "granted": false,
        "reason": "source_changed",
    })
}

/// The previous session's final screen as non-blank lines. Best-effort by
/// design: ANY failure — the row never opened a session, the daemon's
/// tombstone was reaped, the daemon cannot answer — degrades to an empty
/// tail, and the finisher prompt says the screen is unavailable rather than
/// the resume failing over it. The previous screen is context, not evidence.
async fn screen_tail_of(client: &ControlClient, session_id: Option<&str>) -> Vec<String> {
    let Some(sid) = session_id else {
        return Vec::new();
    };
    match client.snapshot(sid).await {
        Ok((_, rows, cols, ansi)) => snapshot_text(rows, cols, &ansi)
            .lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.trim().is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// All inputs needed to render a resume worker's final prompt. Keeping the
/// binding assembly here makes the identity invariant testable: `{artifact}`
/// and `{row}` always name the new finisher row, while the source
/// attempt's artifact is recovery context and the new row's parent artifact.
struct ResumePromptInput<'a> {
    stage: &'a thegn_core::config_pipeline::PipelineStage,
    facts: &'a IssueFacts,
    branch: &'a str,
    worktree: &'a str,
    target_artifact: &'a str,
    source_artifact: &'a str,
    row_id: i64,
    source_artifact_exists: bool,
    source_artifact_tracked: bool,
    git_status: &'a str,
    diff_stat: &'a str,
    screen_tail: &'a [String],
}

fn render_resume_prompt(input: &ResumePromptInput<'_>) -> Result<String> {
    let vars = stage_task_vars(
        input.facts,
        input.branch,
        input.worktree,
        &input.stage.name,
        input.target_artifact,
        input.source_artifact,
        input.row_id,
    );
    let stage_prompt =
        crate::stage_prompt::render_stage(&input.stage.name, &input.stage.prompt, &vars)?;
    Ok(pipeline_resume::finisher_prompt(
        &pipeline_resume::FinisherInput {
            stage_name: &input.stage.name,
            stage_prompt: &stage_prompt,
            artifact: input.source_artifact,
            target_artifact: input.target_artifact,
            artifact_exists: input.source_artifact_exists,
            artifact_tracked: input.source_artifact_tracked,
            git_status: input.git_status,
            diff_stat: input.diff_stat,
            screen_tail: input.screen_tail,
        },
    ))
}

/// The `--resume-work` composition (THE-86): turn an eligible failed, parked,
/// queued, or positively exited pipeline row into a fresh finisher dispatch.
/// An unstamped `spawning`/`running` row is refused because its worker may
/// still be live. An exit-0 row with no committed artifact remains eligible:
/// that is precisely the "session exit ≠ done" failure the done gate catches.
/// A row the Lead already closed (`done` / `merged` / `abandoned`) is refused
/// because a finished or retired verdict is not a resume point
/// (`resume_row_checks`).
///
/// The row is the record: its stage, issue, worktree and agent are reused
/// verbatim (which harness a retry should run on is config's business, or a
/// later chunk's flag — the roster already says who ran the stage). Order of
/// operations mirrors [`open_stage`] (D5): the roster row goes in BEFORE the
/// session opens, and every failure after the insert leaves the new row
/// `failed`, never stuck `queued`.
async fn resume_work(
    cfg: &Config,
    client: &ControlClient,
    row_id: i64,
    bind: bool,
    adopt: bool,
    json: bool,
) -> Result<()> {
    use thegn_svc::control::{AgentLaunch, OpenSpec};
    let db = Db::open()?;
    // 1. The row. The offline `resume_preflight` has already refused a miss
    //    without a daemon; re-read here so the whole composition works from
    //    one consistent snapshot.
    let fetched = db.get_dispatch(row_id)?;
    let stage_name = resume_row_checks(row_id, fetched.as_ref())?.to_string();
    let row = fetched.expect("resume_row_checks refused the miss");
    let stage = stage_or_bail(cfg, &stage_name)?;
    // 2. The row's agent is the record (clap conflicts an explicit --agent
    //    with --resume-work).
    let agent_name = row.agent_name.clone();
    // 3. Worktree + branch, resolved exactly as a fresh dispatch resolves
    //    them.
    let wt = crate::cmd::resolve_worktree(Some(row.worktree_path.clone()))
        .to_string_lossy()
        .into_owned();
    let branch = resolve_branch(&db, &wt);
    // 4. Gather everything that belongs to the source attempt before claiming
    //    the finisher: its artifact state and final screen are recovery
    //    context, never the new row's completion target.
    let facts = gather_issue_facts(client, stage, &row.issue_id).await?;
    // 5. Finisher facts: the source row's artifact state (the same filesystem/git
    //    read the done gate applies), the worktree's git state, and the
    //    previous session's final screen.
    let source_vf = crate::cmd::dispatch::verify_facts(&row);
    let git_status = git_out(Path::new(&wt), &["status", "--porcelain"]).unwrap_or_default();
    let diff_stat = git_out(Path::new(&wt), &["diff", "--stat"]).unwrap_or_default();
    let screen_tail = screen_tail_of(client, row.session_id.as_deref()).await;
    // 6. Reconcile the source before competing for a fresh slot. Failed rows
    //    are already terminal/free; queued, parked, and positively exited rows
    //    are deliberately closed as superseded. A compare-and-set preserves a
    //    newer supervisor verdict that lands between preflight and this point.
    // The finisher is a new dispatch, so it must use the same atomic admission
    // policy as `session open --stage`: capacity and equivalent active work are
    // checked inside BEGIN IMMEDIATE with the insert. If another monitor takes
    // the freed slot first, this attempt is safely refused rather than
    // oversubscribing the stage.
    policy_admission(cfg, &agent_name, &stage.name)?;
    let new_row = claim_resume_dispatch(&db, &row, stage, &wt, &agent_name, json)?;
    let artifact = pipeline_run::artifact_path(&row.issue_id, &stage.name, new_row);
    // 6b. Only now is the finisher's row-derived identity known. Render its
    // stage prompt with the NEW row id/artifact and the source artifact as its
    // parent handoff. If rendering fails, close the reservation rather than
    // leaving a queued row which a monitor would try forever.
    let prompt = match render_resume_prompt(&ResumePromptInput {
        stage,
        facts: &facts,
        branch: &branch,
        worktree: &wt,
        target_artifact: &artifact,
        source_artifact: source_vf.artifact.as_deref().unwrap_or(""),
        row_id: new_row,
        source_artifact_exists: source_vf.exists,
        source_artifact_tracked: source_vf.tracked,
        git_status: &git_status,
        diff_stat: &diff_stat,
        screen_tail: &screen_tail,
    }) {
        Ok(prompt) => prompt,
        Err(error) => {
            let _ = db.update_dispatch_status(new_row, AgentDispatchStatus::Failed);
            return Err(error).with_context(|| format!("dispatch {new_row} failed"));
        }
    };
    // 7. Open — the same headless, stage-layered launch a fresh dispatch
    //    builds, seeded with the finisher prompt instead of the bare task.
    let opened = async {
        let spec = OpenSpec {
            automation_origin: None,
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
            worktree: Some(wt.clone()),
            agent: Some(AgentLaunch {
                agent: agent_name,
                prompt,
                // A resume is always headless — an interactive worker is a
                // pane that sits there forever, same as a fresh dispatch.
                headless: Some(true),
                bind_worktree: bind,
                resume: None,
                // A resume opens a NEW session seeded with the finisher
                // prompt — the harness-native `--continue` form is the
                // daemon retry path's mechanism, not this one.
                continue_last: false,
                // The stage name rides along: the daemon layers the stage's
                // `model`/`env`/`permissions` over the resolved agent exactly
                // as for a fresh dispatch.
                stage: Some(stage.name.clone()),
                fork: false,
                native_session_id: None,
            }),
            adopt,
            already_capped: false,
        };
        client.open(&spec).await
    }
    .await;
    match opened {
        Ok(info) => {
            publish_opened_dispatch(&db, client, new_row, &info.id, &artifact).await?;
            if json {
                outln!(
                    "{}",
                    serde_json::json!({
                        "row": new_row,
                        "session": info.id,
                        "stage": stage.name,
                        "artifact": artifact,
                        "issue": row.issue_id,
                        "worktree": wt,
                        "resumed_from": row_id,
                    })
                );
            } else {
                outln!(
                    "dispatch {new_row} → session {} (stage {}, resume of {row_id})",
                    info.id,
                    stage.name
                );
            }
            Ok(())
        }
        Err(e) => {
            // best-effort: the original error is what the operator needs; if
            // even the failed-stamp cannot land (the roster is a cache), the
            // row stays visibly wrong either way and the wrapped error still
            // names it.
            let _ = db.update_dispatch_status(new_row, AgentDispatchStatus::Failed);
            Err(e).with_context(|| format!("dispatch {new_row} failed"))
        }
    }
}

/// Render an ANSI screen repaint to plain rows: feed it to a fresh emulator
/// of the session's geometry and copy the whole pane out, row by row.
pub(crate) fn snapshot_text(rows: u16, cols: u16, ansi: &[u8]) -> String {
    use crate::emulator::PaneEmulator;
    let mut emu = crate::emulator::AlacrittyEmulator::new(rows.max(1), cols.max(1), 0);
    emu.advance(ansi);
    crate::copymode::extract(&emu, &crate::copymode::whole(&emu))
}

#[cfg(test)]
mod snapshot_text_tests {
    use super::snapshot_text;

    #[test]
    fn ansi_repaint_renders_to_plain_rows() {
        // Two rows, a colored word, and a cursor move: only the text survives.
        let ansi = b"\x1b[H\x1b[2Jhello \x1b[31mred\x1b[0m\r\n\x1b[3;1Hthird";
        let t = snapshot_text(4, 20, ansi);
        let rows: Vec<&str> = t.lines().collect();
        assert_eq!(rows[0], "hello red");
        assert_eq!(rows[1], "");
        assert_eq!(rows[2], "third");
        assert!(!t.contains('\x1b'));
    }
}

#[cfg(test)]
mod session_line_tests {
    use super::session_line;
    use thegn_svc::control::SessionInfo;

    /// The liveness token is the line's **second column** (immediately after
    /// the id), so a supervisor can grep a fixed column.
    fn token(line: &str) -> &str {
        line.split("  ").nth(1).expect("a second column")
    }

    fn info(
        exited_at_ms: Option<i64>,
        exit_code: Option<i32>,
        final_state: Option<&str>,
    ) -> SessionInfo {
        SessionInfo {
            id: "sess-1".into(),
            exited_at_ms,
            exit_code,
            final_state: final_state.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn a_live_session_prints_live() {
        assert_eq!(token(&session_line(&info(None, None, None))), "live");
    }

    #[test]
    fn an_exited_session_prints_its_code() {
        assert_eq!(
            token(&session_line(&info(Some(1), Some(0), None))),
            "exited(0)"
        );
        assert_eq!(
            token(&session_line(&info(Some(1), Some(7), None))),
            "exited(7)"
        );
    }

    #[test]
    fn an_unreapable_exit_prints_a_question_mark() {
        // A killed or unreapable child carries no exit code (the daemon's
        // tombstone leaves it `None`).
        assert_eq!(
            token(&session_line(&info(Some(1), None, None))),
            "exited(?)"
        );
    }

    #[test]
    fn the_final_state_word_is_suffixed_when_present() {
        assert_eq!(
            token(&session_line(&info(Some(1), Some(0), Some("done")))),
            "exited(0,done)"
        );
    }
}

#[cfg(test)]
mod open_stage_tests {
    use super::{IssueFacts, open_preflight, policy_admission, stage_or_bail, stage_task_vars};
    use thegn_core::agent_task::render_prompt;
    use thegn_core::config::Config;
    use thegn_core::config_pipeline::PipelineStage;

    fn cfg_with_stage() -> Config {
        let mut cfg = Config::default();
        cfg.pipeline.stages.push(PipelineStage {
            name: "code".into(),
            agent: "claude".into(),
            prompt: "implement {issue_number}".into(),
            ..Default::default()
        });
        cfg
    }

    /// THE-440: a stage whose effective allow-list the harness cannot grant
    /// command-scoped is held before the roster claim, with an actionable
    /// message; a grantable list and an empty list are admitted.
    #[test]
    fn stage_policy_admission_holds_an_unattestable_grant() {
        let mut cfg = cfg_with_stage();
        cfg.pipeline.stages[0].permissions = vec!["Read".into(), "Bash(git *)".into()];
        policy_admission(&cfg, "claude", "code").expect("claude grants command-scoped");
        cfg.pipeline.stages.push(PipelineStage {
            name: "fanout".into(),
            agent: "claude".into(),
            harness: Some("pi".into()),
            permissions: vec!["Read".into()],
            prompt: "x".into(),
            ..Default::default()
        });
        let err = policy_admission(&cfg, "claude", "fanout").expect_err("pi cannot take it");
        let msg = format!("{err:#}");
        assert!(msg.contains("not dispatched"), "{msg}");
        assert!(msg.contains("permission policy hold"), "{msg}");
        assert!(msg.contains("fanout"), "{msg}");
        // No permissions at all ⇒ nothing to admit, on any harness.
        cfg.pipeline.stages[1].permissions.clear();
        policy_admission(&cfg, "claude", "fanout").expect("empty list");
        // An unresolvable agent is left to the launch path (unchanged).
        policy_admission(&cfg, "ghost", "code").expect("not a policy question");
    }

    #[test]
    fn a_mistyped_stage_name_lists_what_is_configured() {
        let err = stage_or_bail(&cfg_with_stage(), "nosuchstage").expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("nosuchstage"), "names the typo: {msg}");
        assert!(msg.contains("code"), "lists the configured stages: {msg}");
    }

    #[test]
    fn an_empty_pipeline_says_so_instead_of_an_empty_list() {
        let err = stage_or_bail(&Config::default(), "code").expect_err("must refuse");
        assert!(
            err.to_string().contains("no stages are configured"),
            "got {err}"
        );
    }

    #[test]
    fn the_stage_lookup_is_answerable_before_connect() {
        // The offline-refusal contract: a config typo is not a daemon problem,
        // so the preflight must refuse without reaching `connect`.
        let err = open_preflight(&cfg_with_stage(), Some("nosuchstage"), None, "", false)
            .expect_err("must refuse");
        assert!(err.to_string().contains("nosuchstage"), "got {err}");
    }

    #[test]
    fn headless_with_a_blank_prompt_is_refused() {
        let err =
            open_preflight(&cfg_with_stage(), None, None, "   ", true).expect_err("must refuse");
        assert!(err.to_string().contains("empty prompt"), "got {err}");
    }

    #[test]
    fn a_promptless_open_stays_a_legal_interactive_launch() {
        // The real and correct use `--headless` must not break.
        open_preflight(&cfg_with_stage(), None, None, "", false).expect("interactive launch");
    }

    #[test]
    fn a_dispatch_does_not_fire_the_headless_check() {
        // A stage's *rendered* prompt is checked in `open_stage`, where the
        // row can be marked `failed` — not here. (`--stage` WITHOUT `--issue`
        // is a plain overlay open and still gets the check above.)
        open_preflight(&cfg_with_stage(), Some("code"), Some("linear:X"), "", true)
            .expect("stage dispatch is headless by construction");
    }

    #[test]
    fn a_dispatch_refuses_an_explicit_prompt() {
        // The template owns the task; --prompt would be silently ignored.
        let err = open_preflight(
            &cfg_with_stage(),
            Some("code"),
            Some("linear:X"),
            "hand-written task",
            false,
        )
        .expect_err("must refuse");
        assert!(
            err.to_string().contains("template owns the task"),
            "got {err}"
        );
    }

    #[test]
    fn an_issue_body_full_of_braces_survives_the_render() {
        // The literal-brace property (chunk 1 pins the engine; this pins the
        // dispatch's var assembly end-to-end over it): a GraphQL-shaped issue
        // body renders verbatim, braces intact — never re-parsed, so a body
        // cannot inject a placeholder.
        let facts = IssueFacts {
            number: "THE-76".into(),
            title: "pipeline v2".into(),
            body: "the query is { issues { nodes { name } } } and {literal} too".into(),
            url: "https://example.test/THE-76".into(),
        };
        let vars = stage_task_vars(
            &facts,
            "tg/the-76",
            "/wt",
            "code",
            ".thegn/pipeline/THE-76/code/1.md",
            "",
            1,
        );
        let out = render_prompt("work {issue_number}: {issue_body}", &vars).expect("renders");
        assert_eq!(
            out,
            "work THE-76: the query is { issues { nodes { name } } } and {literal} too"
        );
    }
}

#[cfg(test)]
mod resume_work_tests {
    use super::{
        IssueFacts, ResumePromptInput, claim_resume_dispatch, render_resume_prompt,
        resume_claim_is_conflict, resume_conflict_payload, resume_row_checks,
    };
    use thegn_core::config_pipeline::PipelineStage;
    use thegn_core::db::{Db, ResumeDispatchConflict};
    use thegn_core::issue::{AgentDispatch, AgentDispatchStatus};
    use thegn_core::store::NotificationStore;

    fn row(stage: Option<&str>) -> AgentDispatch {
        AgentDispatch {
            id: 7,
            issue_id: "linear:X".into(),
            worktree_path: "/wt/a".into(),
            agent_name: "claude".into(),
            dispatched_at_ms: 0,
            status: AgentDispatchStatus::Failed,
            stage: stage.map(str::to_string),
            parent_id: None,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: None,
            exit_code: None,
            exited_at_ms: None,
        }
    }

    #[test]
    fn a_missing_row_is_refused_with_the_set_status_wording() {
        let err = resume_row_checks(7, None).unwrap_err();
        assert_eq!(err.to_string(), "no dispatch with id 7");
    }

    #[test]
    fn a_plain_row_without_a_stage_is_refused_as_not_a_pipeline_row() {
        let err = resume_row_checks(7, Some(&row(None))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("dispatch 7 is not a pipeline row"), "{msg}");
        assert!(
            msg.contains("--resume-work resumes a --stage dispatch"),
            "{msg}"
        );
    }

    #[test]
    fn a_blank_stage_counts_as_no_stage() {
        let err = resume_row_checks(7, Some(&row(Some("   ")))).unwrap_err();
        assert!(err.to_string().contains("is not a pipeline row"));
    }

    #[test]
    fn a_pipeline_row_yields_its_stage_name() {
        assert_eq!(
            resume_row_checks(7, Some(&row(Some("code")))).unwrap(),
            "code"
        );
    }

    #[test]
    fn a_done_row_is_refused_as_a_closed_verdict() {
        let mut r = row(Some("code"));
        r.status = AgentDispatchStatus::Done;
        let err = resume_row_checks(7, Some(&r)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("dispatch 7 is done"), "{msg}");
        assert!(msg.contains("not a resume point"), "{msg}");
    }

    #[test]
    fn merged_and_abandoned_rows_are_refused_too() {
        for status in [AgentDispatchStatus::Merged, AgentDispatchStatus::Abandoned] {
            let mut r = row(Some("code"));
            r.status = status;
            let msg = resume_row_checks(7, Some(&r)).unwrap_err().to_string();
            assert!(msg.contains(status.as_str()), "{msg}");
        }
    }

    #[test]
    fn a_failed_row_still_resumes_and_so_does_a_parked_one() {
        // `failed` is the resume feature's whole point; waiting_human is the
        // human re-driving a parked row. Neither may be refused.
        for status in [
            AgentDispatchStatus::Failed,
            AgentDispatchStatus::WaitingHuman,
        ] {
            let mut r = row(Some("code"));
            r.status = status;
            assert_eq!(
                resume_row_checks(7, Some(&r)).unwrap(),
                "code",
                "{} must stay resumable",
                status.as_str()
            );
        }
    }

    #[test]
    fn an_unstamped_spawning_or_running_row_cannot_spawn_a_second_worker() {
        for status in [AgentDispatchStatus::Spawning, AgentDispatchStatus::Running] {
            let mut r = row(Some("code"));
            r.status = status;
            let msg = resume_row_checks(7, Some(&r)).unwrap_err().to_string();
            assert!(msg.contains("may still be live"), "{msg}");
            assert!(msg.contains("no exit stamp"), "{msg}");
        }
    }

    #[test]
    fn a_positively_exited_running_row_is_eligible_for_a_finisher() {
        let mut r = row(Some("code"));
        r.status = AgentDispatchStatus::Running;
        r.exited_at_ms = Some(123);
        assert_eq!(resume_row_checks(7, Some(&r)).unwrap(), "code");
    }

    #[test]
    fn a_finisher_prompt_targets_its_new_row_not_the_source_attempt() {
        let stage = PipelineStage {
            name: "code".into(),
            prompt: "write {artifact}; report {row}; previous {parent_artifact}".into(),
            ..Default::default()
        };
        let facts = IssueFacts {
            number: "THE-121".into(),
            title: String::new(),
            body: String::new(),
            url: String::new(),
        };
        let prompt = render_resume_prompt(&ResumePromptInput {
            stage: &stage,
            facts: &facts,
            branch: "tg/the-121",
            worktree: "/wt/121",
            target_artifact: ".thegn/pipeline/THE-121/code/9.md",
            source_artifact: ".thegn/pipeline/THE-121/code/7.md",
            row_id: 9,
            source_artifact_exists: true,
            source_artifact_tracked: true,
            git_status: "",
            diff_stat: "",
            screen_tail: &[],
        })
        .unwrap();
        assert!(prompt.contains(
            "write .thegn/pipeline/THE-121/code/9.md; report 9; previous .thegn/pipeline/THE-121/code/7.md"
        ));
        assert!(prompt.contains(
            "This finisher's required handoff artifact is `.thegn/pipeline/THE-121/code/9.md`"
        ));
        assert!(prompt.contains(
            "Leave the handoff artifact at `.thegn/pipeline/THE-121/code/9.md` committed"
        ));
        assert!(!prompt.contains(
            "Leave the handoff artifact at `.thegn/pipeline/THE-121/code/7.md` committed"
        ));
    }

    #[test]
    fn a_parked_source_is_reconciled_before_its_finisher_claims_the_slot() {
        let db = Db::open_memory().unwrap();
        let source = db
            .put_agent_dispatch(thegn_core::issue::NewDispatch {
                stage: Some("code"),
                chunk_path: Some("chunk-1.md"),
                ..thegn_core::issue::NewDispatch::new("linear:THE-121", "/wt/121", "claude")
            })
            .unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::WaitingHuman)
            .unwrap();
        let source_row = db.get_dispatch(source).unwrap().unwrap();
        let stage = PipelineStage {
            name: "code".into(),
            concurrency: 1,
            ..Default::default()
        };

        let finisher = claim_resume_dispatch(&db, &source_row, &stage, "/wt/121", "claude", false)
            .expect("reconciled source frees its slot");
        let source_after = db.get_dispatch(source).unwrap().unwrap();
        let finisher = db.get_dispatch(finisher).unwrap().unwrap();
        assert_eq!(source_after.status, AgentDispatchStatus::Failed);
        assert_eq!(finisher.status, AgentDispatchStatus::Queued);
        assert_eq!(finisher.parent_id, Some(source));
        assert_eq!(finisher.chunk_path.as_deref(), Some("chunk-1.md"));
    }

    #[test]
    fn a_capacity_refusal_rolls_back_source_reconciliation() {
        let db = Db::open_memory().unwrap();
        let source = db
            .put_agent_dispatch(thegn_core::issue::NewDispatch {
                stage: Some("code"),
                ..thegn_core::issue::NewDispatch::new("linear:THE-121", "/wt/121", "claude")
            })
            .unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::WaitingHuman)
            .unwrap();
        db.put_agent_dispatch(thegn_core::issue::NewDispatch {
            stage: Some("code"),
            artifact_path: Some("other.md"),
            ..thegn_core::issue::NewDispatch::new("linear:OTHER", "/wt/other", "claude")
        })
        .unwrap();
        let source_row = db.get_dispatch(source).unwrap().unwrap();
        let stage = PipelineStage {
            name: "code".into(),
            concurrency: 1,
            ..Default::default()
        };

        let error = claim_resume_dispatch(&db, &source_row, &stage, "/wt/121", "claude", false)
            .unwrap_err();
        assert!(
            error.downcast_ref::<crate::cmd::Retryable>().is_some(),
            "capacity refusal must be retryable: {error:#}"
        );
        let error = error.to_string();
        assert!(error.contains("at capacity"), "{error}");
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(db.list_dispatches().unwrap().len(), 2);
        assert_eq!(
            db.get_dispatch(source).unwrap().unwrap().status,
            AgentDispatchStatus::WaitingHuman
        );
        assert!(db.dispatch_notes(source, None, 0).unwrap().is_empty());
    }

    #[test]
    fn a_noncontention_database_error_remains_fatal() {
        let error = anyhow::anyhow!("database is locked");
        assert!(
            !resume_claim_is_conflict(&error),
            "ordinary database failures must retain the fatal exit classification"
        );
        let conflict = anyhow::Error::new(ResumeDispatchConflict::SourceStatusChanged {
            source_id: 7,
            expected: AgentDispatchStatus::WaitingHuman,
            actual: AgentDispatchStatus::Done,
        });
        assert!(resume_claim_is_conflict(&conflict));
    }

    #[test]
    fn a_source_verdict_conflict_has_one_safe_json_refusal_reason() {
        assert_eq!(
            resume_conflict_payload(),
            serde_json::json!({
                "granted": false,
                "reason": "source_changed",
            })
        );
    }

    #[test]
    fn a_concurrent_source_verdict_is_preserved_without_a_finisher() {
        let db = Db::open_memory().unwrap();
        let source = db
            .put_agent_dispatch(thegn_core::issue::NewDispatch {
                stage: Some("code"),
                ..thegn_core::issue::NewDispatch::new("linear:THE-121", "/wt/121", "claude")
            })
            .unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::WaitingHuman)
            .unwrap();
        let stale_source = db.get_dispatch(source).unwrap().unwrap();
        db.update_dispatch_status(source, AgentDispatchStatus::Done)
            .unwrap();
        let stage = PipelineStage {
            name: "code".into(),
            concurrency: 2,
            ..Default::default()
        };

        let error = claim_resume_dispatch(&db, &stale_source, &stage, "/wt/121", "claude", false)
            .unwrap_err();
        assert!(
            error.downcast_ref::<crate::cmd::Retryable>().is_some(),
            "concurrent verdict refusal must be retryable: {error:#}"
        );
        let error = error.to_string();
        assert!(
            error.contains("changed from waiting_human to done"),
            "{error}"
        );
        assert!(error.contains("newer verdict was preserved"), "{error}");
        assert_eq!(db.list_dispatches().unwrap().len(), 1);
        assert_eq!(
            db.get_dispatch(source).unwrap().unwrap().status,
            AgentDispatchStatus::Done
        );
    }
}

/// Every host capability the `thegn` CLI drives **through the control
/// API** (`ControlClient`), by catalog id: the dedicated verbs below plus
/// everything `thegn api call` reaches generically (every non-streaming row
/// of the `API_CALLS` route table — a newly routed verb becomes CLI-callable
/// with no CLI change). DB-direct verbs (`thegn open`, `thegn wt list`)
/// remain excused in `SURFACE_GAPS` only where no route exists.
#[cfg(test)]
pub fn cli_control_caps() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = thegn_svc::control::routes::API_CALLS
        .iter()
        .filter(|(_, method, _)| *method != "WS")
        .map(|(cap, _, _)| *cap)
        .collect();
    // Streaming caps driven by dedicated verbs, not the generic client.
    v.push("sessions.attach"); // thegn attach / session attach
    v.push("events.subscribe"); // thegn events tail
    v.push("launch.preset"); // thegn open --preset (intents mailbox, not a route)
    // Local operator verbs driven by a dedicated `thegn` subcommand (not the
    // generic control client): the debug bundle reads local files directly.
    v.push("doctor.bundle"); // thegn doctor bundle
    v.push("agent.list"); // thegn agent list (config-derived, no daemon)
    v.push("skills.seed"); // thegn skills seed (local, marker-aware filesystem adapter)
    // Secret-broker verbs (THE-66): implemented as local `thegn secret …`
    // subcommands (they touch local custody, not the daemon), so they cover the
    // CLI surface directly rather than via a control route.
    v.extend([
        "secret.set",
        "secret.rm",
        "secret.list",
        "secret.migrate",
        "secret.audit",
        "secret.ssh.rotate",
    ]);
    // Program verbs (THE-33): local `thegn program …` / `thegn wt new --program`
    // subcommands touching the per-profile DB + git, covering the CLI surface
    // directly rather than via a control route. The deprecated `project`
    // spelling is accepted by the same command implementation.
    v.extend([
        "program.list",
        "program.create",
        "program.rename",
        "program.rm",
        "program.assign",
        "program.new_feature",
    ]);
    // CLI-local reads that resolve through the catalog but not the control
    // socket (no HTTP route): `thegn host discover` shells out to the local
    // tailscale client rather than the daemon.
    v.push("host.discover");
    // Container-estate cleanup is a local CLI verb (`thegn sandbox gc/prune`),
    // not a routed control call — declare the CLI surface's coverage of it here.
    v.push("containers.prune");
    // DB-direct read verb (no control-API route): `thegn map` reads the entity
    // index straight from the state DB. The MCP projection is the catalog's
    // other claimed surface for `semantic.map`.
    v.push("semantic.map"); // thegn map
    // Model-proxy verbs (THE-58): local `thegn proxy …` subcommands touching the
    // daemon/local DB, not a control route, so they cover the CLI surface here.
    v.extend([
        "model_proxy.status",
        "model_proxy.stats",
        "model_proxy.start",
        "model_proxy.stop",
    ]);
    // Cross-profile migration is a local, admin-scoped CLI operation rather
    // than a control route, but it is still a catalog-owned CLI surface.
    v.push("sessions.migrate");
    // Pipeline run-completion verbs (THE-76/THE-88): local `thegn dispatch
    // verify|wait|report|note|status`
    // — the first reads the worktree + roster directly, the second composes the
    // routed `sessions.wait`. Neither is a control route, so they cover the CLI
    // surface here rather than through `API_CALLS`.
    v.extend([
        "dispatches.verify",
        "dispatches.wait",
        "dispatches.report",
        "dispatches.note",
        "dispatches.status",
        "autopilot.status",
    ]);
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod catalog_tests {
    use clap::Parser as _;
    use thegn_core::capability::{Surface, coverage_problems};

    #[test]
    fn cli_control_verbs_cover_catalog() {
        let problems = coverage_problems(Surface::Cli, &super::cli_control_caps());
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn removed_browser_subcommand_is_not_advertised_by_clap() {
        assert!(
            crate::Cli::try_parse_from(["thegn", "session", "browse", "http://localhost:3000/",])
                .is_err()
        );
    }
}
