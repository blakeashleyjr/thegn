//! `thegn session …` — drive a *running* pane daemon from the CLI: list its
//! sessions, send terminal input, dump screen snapshots, stream an attach,
//! and inspect relay leases. Every verb is a thin control-API client;
//! with no daemon running they degrade to a clear message (exit 1), never a
//! crash — the spec's "No daemon present" scenario.

use anyhow::Result;
use base64::Engine as _;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_svc::control::client::{AttachControl, ControlAddr, ControlClient};

#[derive(clap::Subcommand, Clone)]
pub enum SessionAction {
    /// List the daemon's live sessions.
    List {
        /// Emit JSON instead of the human table.
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
    /// Command the preview browser (reserved contract slot).
    Browse {
        #[arg(long)]
        session: Option<String>,
        url: String,
    },
    /// Show relay leases (detached sessions kept warm, and until when).
    Leases {
        #[arg(long)]
        json: bool,
    },
}

/// Render one session as the human-table line shared by `session list` and
/// `thegn attach` (no-arg listing).
pub(crate) fn session_line(s: &thegn_svc::control::SessionInfo) -> String {
    let lease = s
        .lease_expires_at
        .map(|at| format!("  lease→{at}"))
        .unwrap_or_default();
    format!(
        "{}  {}x{}  {} client(s)  {}{}{}",
        s.id,
        s.cols,
        s.rows,
        s.attached_clients,
        s.program,
        s.worktree
            .as_deref()
            .map(|w| format!("  [{w}]"))
            .unwrap_or_default(),
        lease
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
        .map(|d| d.as_millis() as i64)
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
        SessionAction::List { json: true }
            | SessionAction::Snapshot { json: true, .. }
            | SessionAction::Wait { json: true, .. }
            | SessionAction::Leases { json: true }
    );
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
        SessionAction::List { json } => {
            let sessions = client.sessions().await?;
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
                        let _ = stream.control.send(AttachControl::Close).await;
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
        SessionAction::Browse { session, url } => {
            // The reserved drive-browser slot: surface the server's verdict.
            let res = client
                .send_browse(session.as_deref(), &url)
                .await
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "ok".into());
            outln!("{res}");
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
    // CLI-local reads that resolve through the catalog but not the control
    // socket (no HTTP route): `thegn host discover` shells out to the local
    // tailscale client rather than the daemon.
    v.push("host.discover");
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod catalog_tests {
    use thegn_core::capability::{Surface, coverage_problems};

    #[test]
    fn cli_control_verbs_cover_catalog() {
        let problems = coverage_problems(Surface::Cli, &super::cli_control_caps());
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
