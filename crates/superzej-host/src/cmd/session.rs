//! `szhost session …` — drive a *running* pane daemon from the CLI: list its
//! sessions, send terminal input, dump screen snapshots, stream an attach,
//! and inspect relay leases. Every verb is a thin control-API client;
//! with no daemon running they degrade to a clear message (exit 1), never a
//! crash — the spec's "No daemon present" scenario.

use anyhow::Result;
use base64::Engine as _;
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::outln;
use superzej_svc::control::client::{AttachControl, ControlAddr, ControlClient};

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
    },
    /// Stream a session's live output to stdout (Ctrl-C detaches; the
    /// session keeps running).
    Attach {
        #[arg(long)]
        session: String,
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Discover the local daemon (registry first, configured socket as fallback)
/// and verify it answers. `Err` carries the user-facing no-daemon message.
async fn connect(cfg: &Config) -> Result<ControlClient> {
    let addr = Db::open()
        .ok()
        .and_then(|db| {
            superzej_svc::control::client::discover(&db, &crate::daemon::scope_key(), now_ms())
        })
        .unwrap_or_else(|| ControlAddr::Unix(crate::daemon::socket_path(&cfg.daemon)));
    let client = ControlClient::new(addr);
    if client.health().await.is_err() {
        anyhow::bail!(
            "no superzej pane daemon is running — start one with `szhost serve`, \
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
                for s in sessions {
                    let lease = s
                        .lease_expires_at
                        .map(|at| format!("  lease→{at}"))
                        .unwrap_or_default();
                    outln!(
                        "{}  {}x{}  {} client(s)  {}{}{}",
                        s.id,
                        s.cols,
                        s.rows,
                        s.attached_clients,
                        s.program,
                        s.worktree.map(|w| format!("  [{w}]")).unwrap_or_default(),
                        lease
                    );
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
        SessionAction::Snapshot { session, json } => {
            let (seq, rows, cols, ansi) = client.snapshot(&session).await?;
            if json {
                outln!(
                    "{}",
                    serde_json::json!({
                        "session": session, "seq": seq, "rows": rows, "cols": cols,
                        "ansi_b64": base64::engine::general_purpose::STANDARD.encode(&ansi),
                    })
                );
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
                use superzej_core::control_wire::EventFrame;
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
