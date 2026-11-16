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
use std::time::{Duration, Instant};
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_core::pipeline_report::display_cell;
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
            crate::cmd::emit_json(&serde_json::json!({ "session": info.id, "row": row }))?;
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
        DEFAULT_TIMEOUT_SECS, TAIL_BYTES, TAIL_LINES, TailRing, WaitStatus, output_tail,
        process_exit_code, strip_ansi, tail_lines,
    };

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
}
