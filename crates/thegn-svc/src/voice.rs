//! Generic argv-backed voice provider.
//!
//! The provider deliberately owns no microphone or model implementation. A
//! capture command produces a WAV and this provider sends that WAV to the
//! configured transcriber command. Both commands are executed directly, never
//! through a shell, and all output is bounded.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use thegn_core::config::VoiceConfig;
use thegn_core::seam::{Availability, Probe, ProbeReport};
use thegn_core::voice::{VoiceCaps, VoiceError, VoiceProvider};

pub const MAX_WAV_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TRANSCRIPT_BYTES: usize = 1024 * 1024;
const TERMINATE_GRACE: Duration = Duration::from_millis(60);
const WORKER_SETTLE: Duration = Duration::from_millis(60);

/// The external command provider selected by `[voice] kind = "command"`.
#[derive(Debug, Clone)]
pub struct CommandVoiceProvider {
    cfg: VoiceConfig,
}

impl CommandVoiceProvider {
    pub fn new(cfg: VoiceConfig) -> Self {
        Self { cfg }
    }

    fn command_available(argv: &[String]) -> bool {
        let Some(program) = argv.first() else {
            return false;
        };
        if program.contains('/') {
            std::path::Path::new(program).is_file()
        } else {
            thegn_core::util::which_path(program).is_some()
        }
    }

    fn availability(argv: &[String], label: &str) -> Availability {
        let Some(program) = argv.first() else {
            return Availability::Unavailable(format!("no {label} configured"));
        };
        if Self::command_available(argv) {
            Availability::Ready
        } else {
            Availability::Unavailable(format!("{label} executable not found: {program}"))
        }
    }
}

impl Probe for CommandVoiceProvider {
    fn probe(&self) -> ProbeReport {
        let capture = Self::availability(&self.cfg.capture_command, "capture command");
        let transcriber = Self::availability(&self.cfg.command, "transcriber command");
        let availability = if capture.is_ready() && transcriber.is_ready() {
            Availability::Ready
        } else {
            let mut missing = Vec::new();
            if let Availability::Unavailable(reason) = capture {
                missing.push(reason);
            }
            if let Availability::Unavailable(reason) = transcriber {
                missing.push(reason);
            }
            Availability::Unavailable(missing.join("; "))
        };
        ProbeReport::new("voice", "command", availability)
            .with_caps(&VoiceCaps::default())
            .note("argv-only external capture and transcription; no bundled STT/audio")
    }
}

impl VoiceProvider for CommandVoiceProvider {
    fn caps(&self) -> VoiceCaps {
        VoiceCaps::default()
    }

    fn transcribe_push_to_talk(&self, wav: &[u8]) -> Result<String, VoiceError> {
        let never_cancel = AtomicBool::new(false);
        self.transcribe_push_to_talk_cancellable(wav, &never_cancel)
    }
}

impl CommandVoiceProvider {
    /// Run one transcription with a cancellation flag and one total deadline.
    pub fn transcribe_push_to_talk_cancellable(
        &self,
        wav: &[u8],
        cancelled: &AtomicBool,
    ) -> Result<String, VoiceError> {
        self.transcribe_push_to_talk_cancellable_with_reaper(wav, cancelled, || {})
    }

    /// Variant that reports when a handed-off child has been waited on.
    pub fn transcribe_push_to_talk_cancellable_with_reaper(
        &self,
        wav: &[u8],
        cancelled: &AtomicBool,
        on_reaped: impl Fn() + Send + Sync + 'static,
    ) -> Result<String, VoiceError> {
        let on_reaped: Arc<dyn Fn() + Send + Sync> = Arc::new(on_reaped);
        if wav.is_empty() {
            return Err(VoiceError::InvalidInput("capture produced no WAV".into()));
        }
        if wav.len() > MAX_WAV_BYTES {
            return Err(VoiceError::InvalidInput(
                "WAV exceeds the 32 MiB limit".into(),
            ));
        }
        if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
            return Err(VoiceError::InvalidInput(
                "capture output is not a RIFF/WAVE file".into(),
            ));
        }
        let Some(program) = self.cfg.command.first() else {
            return Err(VoiceError::NotConfigured);
        };
        let argv = thegn_core::sandbox_cpucap::wrap_background_argv(self.cfg.command.clone());

        if cancelled.load(Ordering::Acquire) {
            return Err(VoiceError::Failed("transcriber cancelled".into()));
        }
        // Start the deadline before process setup/write so stdin cannot extend
        // the configured bound.
        let deadline = Instant::now() + self.cfg.max_duration();
        let mut command = Command::new(&argv[0]);
        crate::plugin::proc::set_process_group(&mut command);
        let mut child = command
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| VoiceError::NotInstalled(format!("{program}: {e}")))?;
        let mut stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_child(child, &on_reaped);
                return Err(VoiceError::Failed("transcriber stdin unavailable".into()));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate_child(child, &on_reaped);
                return Err(VoiceError::Failed("transcriber stdout unavailable".into()));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate_child(child, &on_reaped);
                return Err(VoiceError::Failed("transcriber stderr unavailable".into()));
            }
        };

        // Readers run alongside the child wait so a chatty command cannot
        // deadlock on a full pipe. Each reader stops at its hard cap. The WAV
        // writer is also off-thread: a command that never reads stdin must not
        // block the deadline loop.
        let (out_tx, out_rx) = mpsc::channel();
        let out_thread = thegn_core::thread_name::spawn_named("thegn-stt-out", move || {
            let mut stdout = stdout;
            let _ = out_tx.send(read_capped(&mut stdout, MAX_TRANSCRIPT_BYTES));
        });
        let (err_tx, err_rx) = mpsc::channel();
        let err_thread = thegn_core::thread_name::spawn_named("thegn-stt-err", move || {
            let mut stderr = stderr;
            let _ = err_tx.send(read_capped(&mut stderr, MAX_TRANSCRIPT_BYTES));
        });
        let (write_tx, write_rx) = mpsc::channel();
        let wav = wav.to_vec();
        let write_thread = thegn_core::thread_name::spawn_named("thegn-stt-wav", move || {
            let result = stdin.write_all(&wav).map_err(|e| format!("write WAV: {e}"));
            drop(stdin);
            let _ = write_tx.send(result);
        });
        let workers = TranscriberWorkers {
            write: write_thread,
            out: out_thread,
            err: err_thread,
            write_rx,
            out_rx,
            err_rx,
        };
        let mut written = None;
        let mut output = None;
        let mut error = None;
        let status = loop {
            if cancelled.load(Ordering::Acquire) {
                let terminated = terminate_child(child, &on_reaped);
                let settled = workers.discard(written, output, error);
                return Err(shutdown_error("transcriber cancelled", terminated, settled));
            }
            if Instant::now() >= deadline {
                let terminated = terminate_child(child, &on_reaped);
                let settled = workers.discard(written, output, error);
                return Err(shutdown_error("transcriber timed out", terminated, settled));
            }
            if written.is_none() {
                written = try_recv(&workers.write_rx);
                if let Some(Err(reason)) = written.as_ref() {
                    let reason = reason.clone();
                    let terminated = terminate_child(child, &on_reaped);
                    let settled = workers.discard(written, output, error);
                    if !terminated || !settled {
                        return Err(VoiceError::UnresolvedProcess(reason));
                    }
                    return Err(VoiceError::Failed(reason));
                }
            }
            if output.is_none() {
                output = try_recv(&workers.out_rx);
                if let Some(Err(reason)) = output.as_ref() {
                    let reason = reason.clone();
                    let terminated = terminate_child(child, &on_reaped);
                    let settled = workers.discard(written, output, error);
                    if !terminated || !settled {
                        return Err(VoiceError::UnresolvedProcess(reason));
                    }
                    return Err(VoiceError::Failed(reason));
                }
            }
            if error.is_none() {
                error = try_recv(&workers.err_rx);
                if let Some(Err(reason)) = error.as_ref() {
                    let reason = reason.clone();
                    let terminated = terminate_child(child, &on_reaped);
                    let settled = workers.discard(written, output, error);
                    if !terminated || !settled {
                        return Err(VoiceError::UnresolvedProcess(
                            "transcriber output exceeded or failed".into(),
                        ));
                    }
                    return Err(VoiceError::Failed(reason));
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    let terminated = terminate_child(child, &on_reaped);
                    let settled = workers.discard(written, output, error);
                    if !terminated || !settled {
                        return Err(VoiceError::UnresolvedProcess(format!(
                            "wait for transcriber: {e}"
                        )));
                    }
                    return Err(VoiceError::Failed(format!("wait for transcriber: {e}")));
                }
            }
        };
        // A command may have exited while a descendant still owns one of the
        // pipes. Close that whole process group before joining helpers so an
        // otherwise successful child cannot leave a reader or writer behind.
        if !terminate_child(child, &on_reaped) {
            if !workers.discard(written, output, error) {
                tracing::warn!("voice transcriber pipe workers did not settle after child exit");
            }
            return Err(VoiceError::UnresolvedProcess(
                "transcriber process tree remained live after termination".into(),
            ));
        }
        let (out, err) = workers.join(written, output, error)?;
        if !status.success() {
            let detail = String::from_utf8_lossy(&err).trim().to_string();
            return Err(VoiceError::Failed(if detail.is_empty() {
                format!("transcriber exited with {status}")
            } else {
                detail
            }));
        }
        String::from_utf8(out)
            .map_err(|_| VoiceError::Failed("transcriber output is not UTF-8".into()))
    }
}

struct TranscriberWorkers {
    write: thread::JoinHandle<()>,
    out: thread::JoinHandle<()>,
    err: thread::JoinHandle<()>,
    write_rx: Receiver<Result<(), String>>,
    out_rx: Receiver<Result<Vec<u8>, String>>,
    err_rx: Receiver<Result<Vec<u8>, String>>,
}

impl TranscriberWorkers {
    fn discard(
        self,
        written: Option<Result<(), String>>,
        output: Option<Result<Vec<u8>, String>>,
        error: Option<Result<Vec<u8>, String>>,
    ) -> bool {
        self.join(written, output, error).is_ok()
    }

    fn join(
        self,
        written: Option<Result<(), String>>,
        output: Option<Result<Vec<u8>, String>>,
        error: Option<Result<Vec<u8>, String>>,
    ) -> Result<(Vec<u8>, Vec<u8>), VoiceError> {
        let Self {
            write,
            out,
            err,
            write_rx,
            out_rx,
            err_rx,
        } = self;
        let deadline = Instant::now() + WORKER_SETTLE;
        while Instant::now() < deadline
            && !(write.is_finished() && out.is_finished() && err.is_finished())
        {
            thread::sleep(Duration::from_millis(2));
        }
        if !(write.is_finished() && out.is_finished() && err.is_finished()) {
            drop((write, out, err));
            return Err(VoiceError::UnresolvedProcess(
                "transcriber pipe worker did not stop before its deadline".into(),
            ));
        }
        let write_joined = write.join().is_ok();
        let out_joined = out.join().is_ok();
        let err_joined = err.join().is_ok();
        if !write_joined {
            return Err(VoiceError::Failed(
                "transcriber input writer panicked".into(),
            ));
        }
        if !out_joined {
            return Err(VoiceError::Failed(
                "transcriber output reader panicked".into(),
            ));
        }
        if !err_joined {
            return Err(VoiceError::Failed(
                "transcriber error reader panicked".into(),
            ));
        }
        written
            .or_else(|| write_rx.recv().ok())
            .ok_or_else(|| VoiceError::Failed("transcriber input writer did not report".into()))?
            .map_err(VoiceError::Failed)?;
        let out = output
            .or_else(|| out_rx.recv().ok())
            .ok_or_else(|| VoiceError::Failed("transcriber output reader did not report".into()))?
            .map_err(VoiceError::Failed)?;
        let err = error
            .or_else(|| err_rx.recv().ok())
            .ok_or_else(|| VoiceError::Failed("transcriber error reader did not report".into()))?
            .map_err(VoiceError::Failed)?;
        Ok((out, err))
    }
}

fn try_recv<T>(rx: &Receiver<Result<T, String>>) -> Option<Result<T, String>> {
    rx.try_recv().ok()
}

fn terminate_child(mut child: Child, on_reaped: &Arc<dyn Fn() + Send + Sync>) -> bool {
    // The command is isolated in the plugin process seam's process group/job,
    // so descendants cannot retain our pipes after cancellation or timeout.
    crate::plugin::proc::kill_group(child.id());
    if let Err(kill_error) = child.kill() {
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => tracing::warn!(%kill_error, "voice transcriber child kill failed"),
            Err(wait_error) => {
                tracing::warn!(%kill_error, %wait_error, "voice transcriber child kill and wait failed")
            }
        }
    }
    let deadline = Instant::now() + TERMINATE_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                on_reaped();
                return true;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
            Ok(None) | Err(_) => break,
        }
    }
    let reaped = Arc::clone(on_reaped);
    match thread::Builder::new()
        .name("voice-transcriber-reaper".into())
        .spawn(move || {
            // thegn-svc has no QoS seam; reaper scheduling follows the platform default.
            if let Err(error) = child.wait() {
                tracing::warn!(%error, "voice transcriber reaper failed");
            } else {
                reaped();
            }
        }) {
        Ok(_) => false,
        Err(error) => {
            // Reaper creation failure is returned as UnresolvedProcess; Child
            // drops here as the documented degraded last resort rather than
            // extending this API beyond its cleanup deadline.
            tracing::error!(%error, "voice transcriber child could not be handed to reaper");
            false
        }
    }
}

fn shutdown_error(context: &str, terminated: bool, settled: bool) -> VoiceError {
    if terminated && settled {
        VoiceError::Failed(context.into())
    } else {
        VoiceError::UnresolvedProcess(format!("{context}; termination or worker join unresolved"))
    }
}

fn read_capped(reader: &mut impl Read, cap: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    reader
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() > cap {
        return Err(format!("command output exceeds {cap} byte limit"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn cfg(command: &[&str]) -> VoiceConfig {
        VoiceConfig {
            enabled: true,
            capture_command: vec!["sh".into()],
            command: command.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn probe_reports_unconfigured_commands() {
        let report = CommandVoiceProvider::new(VoiceConfig::default()).probe();
        assert!(report.availability.is_unavailable());
        assert!(report.notes.iter().any(|n| n.contains("no bundled STT")));
    }

    #[test]
    fn transcriber_is_argv_only_and_returns_utf8() {
        let provider =
            CommandVoiceProvider::new(cfg(&["sh", "-c", "cat >/dev/null; printf ' hello '"]));
        let wav = b"RIFF0000WAVE";
        assert_eq!(provider.transcribe_push_to_talk(wav).unwrap(), " hello ");
    }

    #[test]
    fn successful_transcription_terminates_a_redirected_group_descendant() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let command = [
            "sh",
            "-c",
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! >\"$1\"; cat >/dev/null; printf hi; exec >&- 2>&-; sleep 0.05",
            "sh",
        ];
        let mut command = command.to_vec();
        command.push(pid_file.to_str().unwrap());
        let provider = CommandVoiceProvider::new(cfg(&command));
        assert_eq!(
            provider.transcribe_push_to_talk(b"RIFF0000WAVE").unwrap(),
            "hi"
        );
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let started = Instant::now();
        loop {
            let state = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => stat
                    .rsplit_once(')')
                    .and_then(|(_, tail)| tail.trim_start().chars().next()),
                Err(_) => None,
            };
            if state.is_none() || state == Some('Z') {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "descendant {pid} remains live"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn malformed_capture_is_rejected_before_spawn() {
        let provider = CommandVoiceProvider::new(cfg(&["sh", "-c", "cat"]));
        let err = provider.transcribe_push_to_talk(b"not wav").unwrap_err();
        assert!(matches!(err, VoiceError::InvalidInput(_)));
    }

    #[test]
    fn cancellation_terminates_a_transcriber_and_joins_pipe_workers() {
        let provider = CommandVoiceProvider::new(cfg(&["sh", "-c", "cat >/dev/null; sleep 30"]));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancelled.clone();
        let worker = thread::spawn(move || {
            provider.transcribe_push_to_talk_cancellable(b"RIFF0000WAVE", &worker_cancel)
        });
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        cancelled.store(true, Ordering::Release);
        let err = worker.join().unwrap().unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn excessive_output_terminates_the_child_before_the_deadline() {
        let provider = CommandVoiceProvider::new(cfg(&["sh", "-c", "yes x"]));
        let start = Instant::now();
        let err = provider
            .transcribe_push_to_talk(b"RIFF0000WAVE")
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("output exceeds"));
    }

    #[test]
    fn timeout_terminates_a_transcriber_that_never_reads_stdin() {
        let mut config = cfg(&["sh", "-c", "sleep 30"]);
        config.max_seconds = 1;
        let provider = CommandVoiceProvider::new(config);
        let start = Instant::now();
        let err = provider
            .transcribe_push_to_talk(b"RIFF0000WAVE")
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn failed_termination_or_worker_settle_is_reported_as_unresolved() {
        assert!(matches!(
            shutdown_error("transcriber timed out", false, true),
            VoiceError::UnresolvedProcess(_)
        ));
        assert!(matches!(
            shutdown_error("transcriber cancelled", true, false),
            VoiceError::UnresolvedProcess(_)
        ));
        assert!(matches!(
            shutdown_error("transcriber cancelled", true, true),
            VoiceError::Failed(_)
        ));
    }

    #[test]
    fn stalled_pipe_worker_does_not_make_provider_cleanup_unbounded() {
        let (write_tx, write_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        let write = thread::spawn(|| thread::sleep(Duration::from_millis(300)));
        let out = thread::spawn(|| {});
        let err = thread::spawn(|| {});
        drop((write_tx, out_tx, err_tx));
        let workers = TranscriberWorkers {
            write,
            out,
            err,
            write_rx,
            out_rx,
            err_rx,
        };
        let start = Instant::now();
        assert!(!workers.discard(None, None, None));
        assert!(start.elapsed() < Duration::from_millis(200));
    }
}
