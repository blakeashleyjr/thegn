//! Generic argv-backed voice provider.
//!
//! The provider deliberately owns no microphone or model implementation. A
//! capture command produces a WAV and this provider sends that WAV to the
//! configured transcriber command. Both commands are executed directly, never
//! through a shell, and all output is bounded.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thegn_core::config::VoiceConfig;
use thegn_core::seam::{Availability, Probe, ProbeReport};
use thegn_core::voice::{VoiceCaps, VoiceError, VoiceProvider};

pub const MAX_WAV_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TRANSCRIPT_BYTES: usize = 1024 * 1024;

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

        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| VoiceError::NotInstalled(format!("{program}: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| VoiceError::Failed("transcriber stdin unavailable".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| VoiceError::Failed("transcriber stdout unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| VoiceError::Failed("transcriber stderr unavailable".into()))?;

        // Readers run alongside the child wait so a chatty command cannot
        // deadlock on a full pipe. Each reader stops at its hard cap.
        let out_thread = thread::spawn(move || read_capped(&mut stdout, MAX_TRANSCRIPT_BYTES));
        let err_thread = thread::spawn(move || read_capped(&mut stderr, MAX_TRANSCRIPT_BYTES));
        stdin
            .write_all(wav)
            .map_err(|e| VoiceError::Failed(format!("write WAV: {e}")))?;
        drop(stdin);

        let deadline = Instant::now() + self.cfg.max_duration();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_thread.join();
                    let _ = err_thread.join();
                    return Err(VoiceError::Failed("transcriber timed out".into()));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(VoiceError::Failed(format!("wait for transcriber: {e}")));
                }
            }
        };
        let out = out_thread
            .join()
            .map_err(|_| VoiceError::Failed("transcriber output reader panicked".into()))?
            .map_err(VoiceError::Failed)?;
        let err = err_thread
            .join()
            .map_err(|_| VoiceError::Failed("transcriber error reader panicked".into()))?
            .map_err(VoiceError::Failed)?;
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
    fn malformed_capture_is_rejected_before_spawn() {
        let provider = CommandVoiceProvider::new(cfg(&["sh", "-c", "cat"]));
        let err = provider.transcribe_push_to_talk(b"not wav").unwrap_err();
        assert!(matches!(err, VoiceError::InvalidInput(_)));
    }
}
