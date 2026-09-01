//! `thegn events tail` — a thin client for the daemon's live event feed.
//!
//! The daemon owns filtering, authorization, bounded delivery, and lag
//! signaling. This command only validates the shared filter and formats the
//! frames returned by the typed control client; it never polls or enables
//! session input.

use anyhow::{Result, anyhow};
use clap::Subcommand;
use thegn_core::config::Config;
use thegn_core::control_wire::{EventFrame, FeedFilter};
use thegn_core::outln;
use thegn_svc::control::client::frame_json;

#[derive(Subcommand, Clone)]
pub enum Action {
    /// Stream the daemon's live event feed until it disconnects.
    Tail {
        /// Comma-separated event kinds to receive. The greeting is always
        /// delivered first; valid kinds are shown in the control schema.
        #[arg(long)]
        kinds: Option<String>,
        /// Only session-keyed events for this session.
        #[arg(long)]
        session: Option<String>,
        /// Emit a visible frame when the bounded feed skips events.
        #[arg(long)]
        signal_lag: bool,
        /// Emit one canonical JSON frame per line (NDJSON).
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(cfg, action))
}

async fn run_async(cfg: &Config, action: Action) -> Result<()> {
    let Action::Tail {
        kinds,
        session,
        signal_lag,
        json,
    } = action;
    let filter = FeedFilter::parse(kinds.as_deref(), session.as_deref(), signal_lag)
        .map_err(|e| anyhow!("invalid event filter: {e}"))?;

    let client = match crate::cmd::session::connect(cfg).await {
        Ok(client) => client,
        Err(error) => {
            if json {
                outln!("{}", serde_json::json!({ "error": "no_daemon" }));
            }
            return Err(error);
        }
    };
    let mut stream = client.subscribe_events_opts(&filter).await?;
    while let Some(frame) = stream.frames.recv().await {
        outln!("{}", format_frame(&frame, json)?);
    }
    Ok(())
}

/// Format every feed frame through the shared client JSON projection. JSON
/// mode is deliberately one compact document per frame so it can be piped to
/// `jq` while the human mode remains useful at a terminal.
pub(crate) fn format_frame(frame: &EventFrame, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string(&frame_json(frame))?);
    }
    Ok(match frame {
        EventFrame::Hello(hello) => format!(
            "hello proto={} server={} scopes={}",
            hello.proto,
            hello.server,
            hello
                .scopes
                .iter()
                .map(|scope| scope.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ),
        EventFrame::PaneSnapshot {
            session,
            seq,
            cols,
            rows,
            bytes,
        } => format!(
            "snapshot session={session} seq={seq} size={cols}x{rows} bytes={}",
            bytes.len()
        ),
        EventFrame::PaneDelta {
            session,
            seq,
            bytes,
        } => format!("delta session={session} seq={seq} bytes={}", bytes.len()),
        EventFrame::Activity { json } => format!("activity {json}"),
        EventFrame::Lease {
            session,
            kind,
            expires_at,
        } => format!(
            "lease session={session} event={} expires_at={}",
            lease_kind_name(*kind),
            expires_at.map_or_else(|| "-".into(), |at| at.to_string())
        ),
        EventFrame::Pairing {
            pairing_id,
            label,
            scope,
            state,
        } => format!(
            "pairing id={pairing_id} label={label} scope={scope} state={}",
            pairing_state_name(*state)
        ),
        EventFrame::Sessions => "sessions changed".into(),
        EventFrame::SessionExit { session, code } => format!(
            "exit session={session} code={}",
            code.map_or_else(|| "?".into(), |code| code.to_string())
        ),
        EventFrame::Lagged { missed } => format!("[lagged: missed {missed} frame(s)]"),
    })
}

fn lease_kind_name(kind: thegn_core::control_wire::LeaseEventKind) -> &'static str {
    match kind {
        thegn_core::control_wire::LeaseEventKind::Opened => "opened",
        thegn_core::control_wire::LeaseEventKind::Refreshed => "refreshed",
        thegn_core::control_wire::LeaseEventKind::Released => "released",
        thegn_core::control_wire::LeaseEventKind::Reaped => "reaped",
    }
}

fn pairing_state_name(state: thegn_core::control_wire::PairingState) -> &'static str {
    match state {
        thegn_core::control_wire::PairingState::Requested => "requested",
        thegn_core::control_wire::PairingState::Approved => "approved",
        thegn_core::control_wire::PairingState::Revoked => "revoked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use thegn_core::control::Scope;
    use thegn_core::control_wire::{Hello, LeaseEventKind, PairingState};

    #[test]
    fn human_formatter_keeps_greeting_and_lag_visible() {
        let hello = EventFrame::Hello(Hello {
            proto: 1,
            server: "daemon".into(),
            scopes: vec![Scope::Read],
        });
        assert_eq!(
            format_frame(&hello, false).unwrap(),
            "hello proto=1 server=daemon scopes=read"
        );
        assert_eq!(
            format_frame(&EventFrame::Lagged { missed: 7 }, false).unwrap(),
            "[lagged: missed 7 frame(s)]"
        );
    }

    #[test]
    fn json_formatter_uses_the_canonical_frame_shape() {
        let frame = EventFrame::Pairing {
            pairing_id: "p1".into(),
            label: "laptop".into(),
            scope: "read".into(),
            state: PairingState::Approved,
        };
        let value: serde_json::Value =
            serde_json::from_str(&format_frame(&frame, true).unwrap()).unwrap();
        assert_eq!(value["kind"], "pairing");
        assert_eq!(value["state"], "approved");
    }

    #[test]
    fn all_human_frame_variants_are_renderable() {
        let frames = [
            EventFrame::Hello(Hello {
                proto: 1,
                server: "daemon".into(),
                scopes: vec![Scope::Read],
            }),
            EventFrame::PaneSnapshot {
                session: "s".into(),
                seq: 1,
                cols: 80,
                rows: 24,
                bytes: vec![1],
            },
            EventFrame::PaneDelta {
                session: "s".into(),
                seq: 2,
                bytes: vec![2],
            },
            EventFrame::Activity { json: "{}".into() },
            EventFrame::Lease {
                session: "s".into(),
                kind: LeaseEventKind::Opened,
                expires_at: None,
            },
            EventFrame::Pairing {
                pairing_id: "p".into(),
                label: "l".into(),
                scope: "read".into(),
                state: PairingState::Requested,
            },
            EventFrame::Sessions,
            EventFrame::SessionExit {
                session: "s".into(),
                code: Some(0),
            },
            EventFrame::Lagged { missed: 1 },
        ];
        for frame in frames {
            assert!(!format_frame(&frame, false).unwrap().is_empty());
        }
    }

    #[test]
    fn parser_uses_the_shared_filter_vocabulary() {
        let filter = FeedFilter::parse(Some("activity,exit"), Some("s1"), true).unwrap();
        assert_eq!(filter.kinds, Some(vec!["activity".into(), "exit".into()]));
        assert_eq!(filter.session.as_deref(), Some("s1"));
        assert!(filter.signal_lag);
    }

    #[test]
    fn top_level_parser_exposes_events_tail_flags() {
        let cli = crate::Cli::try_parse_from([
            "thegn",
            "events",
            "tail",
            "--kinds",
            "activity,exit",
            "--session",
            "s1",
            "--signal-lag",
            "--json",
        ])
        .unwrap();
        let Some(crate::Command::Events {
            action:
                Action::Tail {
                    kinds,
                    session,
                    signal_lag,
                    json,
                },
        }) = cli.command
        else {
            panic!("events tail did not parse as the events command");
        };
        assert_eq!(kinds.as_deref(), Some("activity,exit"));
        assert_eq!(session.as_deref(), Some("s1"));
        assert!(signal_lag && json);
    }

    #[test]
    fn missing_daemon_returns_the_shared_recoverable_error() {
        let state = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _env = crate::testenv::EnvVarGuard::set(&[
            ("XDG_STATE_HOME", state.path().to_str().unwrap()),
            ("XDG_RUNTIME_DIR", runtime.path().to_str().unwrap()),
        ]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(crate::cmd::session::connect(&Config::default()));
        let error = match result {
            Ok(_) => panic!("fresh state must not discover a daemon"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("no thegn pane daemon is running")
        );
    }
}
