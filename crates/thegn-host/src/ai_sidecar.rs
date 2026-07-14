//! The AI-metrics sidecar spawn, extracted from `run.rs` (pinned by the
//! file-size ratchet): a detached task that reads JSON metric lines from the
//! python sidecar's stdout and feeds them to the loop over the metrics
//! channel + a waker pulse.

pub fn spawn_ai_sidecar(
    waker: termwiz::terminal::TerminalWaker,
    tx: tokio::sync::mpsc::UnboundedSender<crate::chrome::AiMetrics>,
) {
    tokio::spawn(async move {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::process::Command;

        let mut child = match Command::new("python3")
            .arg("src/sidecar.py")
            .stdout(Stdio::piped())
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
