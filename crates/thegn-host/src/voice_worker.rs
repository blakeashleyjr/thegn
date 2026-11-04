//! Private, version-local worker protocol. Credentials travel over a pipe,
//! never argv, environment overrides, logs, or a recording on disk.
#[cfg(feature = "voice-native")]
use anyhow::Context;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::io::BufRead;
#[cfg(feature = "voice-native")]
use std::io::Write;
use thegn_core::config::VoiceConfig;

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub cfg: VoiceConfig,
    pub token: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Event {
    /// Native capture for a legacy command; the parent owns the command child
    /// lifecycle so killing the native worker cannot orphan a transcriber.
    Captured(String),
    Timing {
        final_pass: bool,
        decode_ms: u64,
        audio_end_ms: u64,
    },
    Partial(String),
    Stopped,
    Final(String),
    Failed(String),
}
pub(crate) const MAX_EVENT: u64 = 16 * 1024 * 1024;

pub(crate) fn read_line(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    let n = std::io::Read::take(&mut *reader, MAX_EVENT + 1).read_line(&mut line)?;
    if n as u64 > MAX_EVENT {
        bail!("voice worker message exceeds protocol limit");
    }
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        bail!("voice worker message interrupted");
    }
    Ok(Some(line))
}

pub(crate) fn run() -> Result<()> {
    #[cfg(feature = "voice-native")]
    {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let mut input = std::io::BufReader::new(std::io::stdin());
        let request: Request =
            serde_json::from_str(&read_line(&mut input)?.context("missing voice worker request")?)?;
        if !request.cfg.enabled {
            bail!("voice is disabled");
        }
        let result = session(request, input);
        if let Err(e) = result {
            emit(Event::Failed(format!("{e:#}")))?;
        }
        Ok(())
    }
    #[cfg(not(feature = "voice-native"))]
    bail!(
        "native voice is not compiled; rebuild with --features voice-native (voice-metal on Apple silicon)"
    )
}

#[cfg(feature = "voice-native")]
fn emit(event: Event) -> Result<()> {
    let encoded = serde_json::to_vec(&event)?;
    if encoded.len() as u64 >= MAX_EVENT {
        bail!("voice worker transcript exceeds protocol limit");
    }
    let mut out = std::io::stdout().lock();
    out.write_all(&encoded)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

#[cfg(feature = "voice-native")]
fn session(request: Request, mut input: impl BufRead + Send + 'static) -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};
    use thegn_core::config::VoiceKind;
    use thegn_voice::{PREVIEW_STEP, RollingTranscript, WINDOW_SAMPLES};
    let cfg = request.cfg;
    let stop = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let cancelled = cancel.clone();
    let _control = thegn_core::thread_name::try_spawn_named("thegn-voice-ctl", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        match read_line(&mut input) {
            Ok(Some(line)) if line.trim() == "stop" => {}
            _ => {
                cancelled.store(true, Ordering::Release);
            }
        }
        stopped.store(true, Ordering::Release);
    })?;
    // Model acquisition is explicitly CLI-only. Never download or open a mic
    // until setup has installed the selected artifact.
    let engine = if cfg.kind == VoiceKind::Embedded {
        thegn_voice::whisper::Engine::validate_language(&cfg.language)?;
        let path = thegn_svc::voice_models::resolve(&cfg)?;
        anyhow::ensure!(
            path.is_file(),
            "voice model missing; run thegn voice setup before recording"
        );
        if cfg.model_path.is_none() {
            thegn_svc::voice_models::verify(
                &path,
                thegn_svc::voice_models::model(&cfg.model)?,
                &cancel,
            )?;
        }
        Some(thegn_voice::whisper::Engine::load(&path)?)
    } else {
        None
    };
    if cfg.kind == VoiceKind::Http {
        thegn_svc::voice_http::validate_endpoint(&cfg.endpoint)?;
        cfg.validate_credential_transport()
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            !cfg.model.is_empty(),
            "HTTP model is missing; run thegn voice setup --kind http"
        );
    }
    if cfg.kind == VoiceKind::Command && cfg.command.is_empty() {
        bail!("no transcriber command configured");
    }
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    if stop.load(Ordering::Acquire) {
        return emit(Event::Final(String::new()));
    }
    let mut capture =
        thegn_voice::capture::Capture::start(cfg.effective_max_seconds(), stop.clone(), || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility)
        })?;
    let mut rolling = RollingTranscript::default();
    let mut decoded_end = 0;
    let transcribe = |samples: &[f32], flag: Arc<AtomicBool>| -> Result<String> {
        match cfg.kind {
            VoiceKind::Embedded => engine
                .as_ref()
                .context("missing embedded engine")?
                .transcribe(samples, &cfg.language, flag, cfg.request_timeout()),
            VoiceKind::Http => Ok(thegn_svc::voice_http::transcribe(
                &cfg,
                request.token.as_deref(),
                &thegn_voice::wav(samples),
                &flag,
            )?),
            VoiceKind::Command => bail!("command transcription belongs to the parent adapter"),
        }
    };
    loop {
        if cancel.load(Ordering::Acquire) {
            capture.close()?;
            return Ok(());
        }
        let (end, closed) = capture.progress()?;
        if stop.load(Ordering::Acquire) || closed {
            break;
        }
        if cfg.live_preview && cfg.kind != VoiceKind::Command && end >= decoded_end + PREVIEW_STEP {
            let (start, samples, _) = capture.snapshot(Some(WINDOW_SAMPLES))?;
            let end = start + samples.len();
            let began = Instant::now();
            // The stop flag aborts a preview to make room for the final pass.
            // No queue: after decoding, snapshot the newest window directly.
            match transcribe(&samples, stop.clone()) {
                Ok(text) if !stop.load(Ordering::Acquire) => {
                    let text = rolling.update(start, &text);
                    emit(Event::Partial(thegn_voice::preview_snippet(&text).into()))?;
                    emit(Event::Timing {
                        final_pass: false,
                        decode_ms: began.elapsed().as_millis() as u64,
                        audio_end_ms: (end * 1000 / 16000) as u64,
                    })?;
                }
                Err(e) if !stop.load(Ordering::Acquire) => {
                    capture.close()?;
                    return Err(e);
                }
                _ => {}
            }
            decoded_end = end;
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    capture.close()?;
    if cfg.kind == VoiceKind::Command {
        use base64::Engine;
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let (_, samples, _) = capture.snapshot(None)?;
        return emit(Event::Captured(
            base64::engine::general_purpose::STANDARD.encode(thegn_voice::wav(&samples)),
        ));
    }
    emit(Event::Stopped)?;
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let (_, samples, _) = capture.snapshot(None)?;
    let began = Instant::now();
    let text = transcribe(&samples, cancel)?;
    emit(Event::Timing {
        final_pass: true,
        decode_ms: began.elapsed().as_millis() as u64,
        audio_end_ms: (samples.len() * 1000 / 16000) as u64,
    })?;
    emit(Event::Final(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn voice_protocol_rejects_truncated_and_excessive_events() {
        assert_eq!(
            read_line(&mut &b"hello\n"[..]).unwrap().as_deref(),
            Some("hello\n")
        );
        assert!(read_line(&mut &b""[..]).unwrap().is_none());
        assert!(read_line(&mut &b"truncated"[..]).is_err());
        assert!(read_line(&mut &vec![b'x'; MAX_EVENT as usize + 1][..]).is_err());
    }
}
