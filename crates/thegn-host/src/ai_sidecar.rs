//! The AI-metrics sidecar spawn, extracted from `run.rs` (pinned by the
//! file-size ratchet): a detached task that reads JSON metric lines from the
//! sidecar's stdout and feeds them to the loop over the metrics channel + a
//! waker pulse.
//!
//! The AI layer is strictly additive (see CLAUDE.md), so this is **opt-in**: it
//! spawns nothing unless `THEGN_AI_SIDECAR` names the script to run. Its value
//! must be an explicit path — the previous unconditional `python3 src/sidecar.py`
//! resolved against the *current working directory*, so launching thegn inside
//! any repo that happened to ship `src/sidecar.py` executed that repo-controlled
//! code on the host, unsandboxed.

/// Env var naming the AI-metrics sidecar script (absolute path recommended). The
/// feature is entirely off when it is unset or empty.
const SIDECAR_ENV: &str = "THEGN_AI_SIDECAR";

pub fn spawn_ai_sidecar(
    waker: termwiz::terminal::TerminalWaker,
    tx: tokio::sync::mpsc::UnboundedSender<crate::chrome::AiMetrics>,
) {
    let Some(script) = std::env::var(SIDECAR_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        // Feature off: no sidecar configured. This is the default.
        return;
    };
    // Refuse a relative path outright: it would resolve against the cwd (whatever
    // repo the user launched from), which is exactly the untrusted-code hazard.
    if !std::path::Path::new(&script).is_absolute() {
        tracing::warn!(
            "{SIDECAR_ENV}={script:?} must be an absolute path; not spawning AI sidecar"
        );
        return;
    }
    tokio::spawn(async move {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::process::Command;

        let mut child = match Command::new("python3")
            .arg(&script)
            .stdout(Stdio::piped())
            // Never inherit stderr: this process owns a raw-mode terminal, and
            // sidecar diagnostics printed to the tty would corrupt the TUI.
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to spawn AI metrics sidecar: {e}");
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Ok(metrics) = serde_json::from_str::<crate::chrome::AiMetrics>(&line) {
                    let _ = tx.send(metrics);
                    let _ = waker.wake();
                }
            }
        }
    });
}
