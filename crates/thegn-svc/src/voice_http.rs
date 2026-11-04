//! Multipart WAV/JSON transcription. No provider fallback or persisted audio.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thegn_core::config::VoiceConfig;
use thegn_core::seam::{Availability, Probe, ProbeReport};
use thegn_core::voice::VoiceError;
use thegn_core::voice::{VoiceCaps, VoiceProvider};

/// Credentials are deliberately excluded from Debug and all probe diagnostics.
pub struct HttpVoiceProvider {
    cfg: VoiceConfig,
    credential: Option<String>,
}
impl HttpVoiceProvider {
    pub fn new(cfg: VoiceConfig, credential: Option<String>) -> Self {
        Self { cfg, credential }
    }
}
impl Probe for HttpVoiceProvider {
    fn probe(&self) -> ProbeReport {
        let availability = match validate_endpoint(&self.cfg.endpoint) {
            Ok(()) if !self.cfg.model.is_empty() => Availability::Ready,
            Ok(()) => Availability::Unavailable("no HTTP transcription model configured".into()),
            Err(e) => Availability::Unavailable(e.to_string()),
        };
        ProbeReport::new("voice", "http", availability)
            .with_caps(&self.caps())
            .note("multipart WAV/JSON; live windows repeat audio billing; no provider fallback")
    }
}
impl VoiceProvider for HttpVoiceProvider {
    fn caps(&self) -> VoiceCaps {
        VoiceCaps {
            transcribe: true,
            partials: self.cfg.live_preview,
            cancellation: true,
        }
    }
    fn transcribe_push_to_talk(&self, wav: &[u8]) -> Result<String, VoiceError> {
        self.transcribe_cancellable(wav, &AtomicBool::new(false))
    }
    fn transcribe_cancellable(
        &self,
        wav: &[u8],
        cancelled: &AtomicBool,
    ) -> Result<String, VoiceError> {
        transcribe(&self.cfg, self.credential.as_deref(), wav, cancelled)
    }
}

pub fn validate_endpoint(endpoint: &str) -> Result<(), VoiceError> {
    let url = url::Url::parse(endpoint)
        .map_err(|_| VoiceError::InvalidInput("invalid HTTP endpoint URL".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(VoiceError::InvalidInput(
            "endpoint must be HTTP(S), without userinfo or fragment".into(),
        ));
    }
    Ok(())
}

/// Drops the HTTP future on cancellation or the total deadline. Response bodies
/// and reqwest errors are never included in diagnostics (may contain credentials).
pub fn transcribe(
    cfg: &VoiceConfig,
    credential: Option<&str>,
    wav: &[u8],
    cancel: &AtomicBool,
) -> Result<String, VoiceError> {
    validate_endpoint(&cfg.endpoint)?;
    if cfg.model.is_empty() {
        return Err(VoiceError::NotConfigured);
    }
    if wav.len() < 12
        || wav.len() > super::voice::MAX_WAV_BYTES
        || &wav[..4] != b"RIFF"
        || &wav[8..12] != b"WAVE"
    {
        return Err(VoiceError::InvalidInput(
            "expected a bounded WAV recording".into(),
        ));
    }
    crate::voice_runtime::run(async {
        let operation = async {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| VoiceError::Failed("HTTP client unavailable".into()))?;
            let file = reqwest::multipart::Part::bytes(wav.to_vec())
                .file_name("dictation.wav")
                .mime_str("audio/wav")
                .map_err(|_| VoiceError::Failed("WAV form unavailable".into()))?;
            let mut form = reqwest::multipart::Form::new()
                .part("file", file)
                .text("model", cfg.model.clone())
                .text("response_format", "json");
            if !cfg.language.is_empty() && cfg.language != "auto" {
                form = form.text("language", cfg.language.clone());
            }
            let mut request = client.post(&cfg.endpoint).multipart(form);
            if let Some(token) = credential {
                request = request.bearer_auth(token);
            }
            let mut response = request.send().await.map_err(|e| {
                if e.is_timeout() {
                    VoiceError::Timeout
                } else {
                    VoiceError::Connection
                }
            })?;
            let status = response.status();
            if !status.is_success() {
                return Err(VoiceError::HttpStatus(status.as_u16()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| VoiceError::Failed("HTTP response interrupted".into()))?
            {
                if body.len() + chunk.len() > super::voice::MAX_TRANSCRIPT_BYTES {
                    return Err(VoiceError::Failed("HTTP response exceeds 1 MiB".into()));
                }
                body.extend_from_slice(&chunk);
            }
            decode_response(&body)
        };
        tokio::pin!(operation);
        let deadline = tokio::time::sleep(cfg.request_timeout());
        tokio::pin!(deadline);
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(VoiceError::Failed("transcription cancelled".into()));
            }
            tokio::select! {
                result = &mut operation => return result,
                _ = &mut deadline => return Err(VoiceError::Timeout),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    })
    .map_err(|_| VoiceError::Failed("HTTP runtime unavailable".into()))?
}

fn decode_response(body: &[u8]) -> Result<String, VoiceError> {
    if body.len() > super::voice::MAX_TRANSCRIPT_BYTES {
        return Err(VoiceError::Failed("HTTP response exceeds 1 MiB".into()));
    }
    #[derive(serde::Deserialize)]
    struct Transcript {
        text: String,
    }
    serde_json::from_slice::<Transcript>(body)
        .map(|v| v.text)
        .map_err(|_| {
            VoiceError::Failed("HTTP response must be JSON with a string text field".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_json_without_echoing_response_or_secrets() {
        assert_eq!(decode_response(br#"{"text":"hello"}"#).unwrap(), "hello");
        for body in [
            br#"{"text":2}"#.as_slice(),
            b"secret-token",
            b"{}",
            &[b'x'; 0],
        ] {
            let error = decode_response(body).unwrap_err().to_string();
            assert!(!error.contains("secret-token"));
        }
        assert!(
            decode_response(&vec![b' '; super::super::voice::MAX_TRANSCRIPT_BYTES + 1]).is_err()
        );
    }
    #[test]
    fn cancellation_precedes_network_work_and_does_not_fall_back() {
        let cfg = VoiceConfig {
            endpoint: "http://127.0.0.1:1/transcribe".into(),
            ..Default::default()
        };
        let error = transcribe(
            &cfg,
            Some("secret-token"),
            b"RIFF0000WAVE",
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!error.to_string().contains("secret-token"));
    }
    #[test]
    fn rejects_non_http_and_inline_credentials() {
        for endpoint in [
            "",
            "file:///tmp/audio",
            "https://user:pass@host/stt",
            "https://host/stt#secret",
        ] {
            assert!(validate_endpoint(endpoint).is_err());
        }
        assert!(validate_endpoint("http://127.0.0.1:8000/v1/audio/transcriptions").is_ok());
    }
}

#[cfg(test)]
mod loopback_tests {
    use super::*;
    use std::io::{Read, Write};

    fn server(
        status: u16,
        body: Vec<u8>,
        stall: bool,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("loopback permission required");
        let endpoint = format!(
            "http://{}/v1/audio/transcriptions",
            listener.local_addr().unwrap()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
                if let Some(i) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..i]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|s| s.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= i + 4 + length {
                        break;
                    }
                }
            }
            if stall {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"text\":\"")
                    .unwrap();
                std::thread::sleep(Duration::from_secs(2));
            } else {
                let header = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if let Err(error) = stream
                    .write_all(header.as_bytes())
                    .and_then(|_| stream.write_all(&body))
                {
                    // An oversized-response fixture intentionally closes early.
                    assert!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ),
                        "{error}"
                    );
                }
            }
            request
        });
        (endpoint, handle)
    }

    #[test]
    fn voice_http_multipart_contract_and_classified_failures() {
        for (status, body, expected) in [
            (200, br#"{"text":"bonjour"}"#.to_vec(), "bonjour"),
            (401, b"secret-token".to_vec(), "authentication"),
            (403, Vec::new(), "authentication"),
            (429, Vec::new(), "rate limit"),
            (500, Vec::new(), "rejected"),
            (302, Vec::new(), "rejected"),
            (200, b"malformed".to_vec(), "JSON"),
            (
                200,
                vec![b'x'; super::super::voice::MAX_TRANSCRIPT_BYTES + 1],
                "exceeds",
            ),
        ] {
            let (endpoint, server) = server(status, body, false);
            let cfg = VoiceConfig {
                endpoint,
                model: "fixture-model".into(),
                language: "fr".into(),
                ..Default::default()
            };
            let result = transcribe(
                &cfg,
                Some("secret-token"),
                b"RIFF0000WAVE",
                &AtomicBool::new(false),
            );
            let text = result.unwrap_or_else(|e| e.to_string());
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains("secret-token"));
            let request = String::from_utf8(server.join().unwrap()).unwrap();
            assert!(request.contains("multipart/form-data"));
            assert!(request.contains("Bearer secret-token"));
            for value in [
                "fixture-model",
                "name=\"language\"",
                "fr",
                "dictation.wav",
                "audio/wav",
                "RIFF0000WAVE",
            ] {
                assert!(request.contains(value), "missing multipart value {value}");
            }
        }
    }
    #[test]
    fn voice_http_stalled_response_deadline_and_cancellation() {
        for cancelled in [false, true] {
            let (endpoint, server) = server(200, Vec::new(), true);
            let cfg = VoiceConfig {
                endpoint,
                timeout_seconds: 1,
                ..Default::default()
            };
            let flag = std::sync::Arc::new(AtomicBool::new(false));
            let cancel = flag.clone();
            let trigger = std::thread::spawn(move || {
                if cancelled {
                    std::thread::sleep(Duration::from_millis(100));
                    cancel.store(true, Ordering::Release);
                }
            });
            let began = std::time::Instant::now();
            let error = transcribe(&cfg, None, b"RIFF0000WAVE", &flag)
                .unwrap_err()
                .to_string();
            assert!(began.elapsed() < Duration::from_millis(1600));
            assert!(error.contains(if cancelled { "cancelled" } else { "timed out" }));
            trigger.join().unwrap();
            server.join().unwrap();
        }
    }

    #[test]
    fn voice_http_refused_connection_is_classified_without_echoing_endpoint() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/secret-path", listener.local_addr().unwrap());
        drop(listener);
        let cfg = VoiceConfig {
            endpoint,
            timeout_seconds: 1,
            ..Default::default()
        };
        let error = transcribe(&cfg, None, b"RIFF0000WAVE", &AtomicBool::new(false)).unwrap_err();
        assert_eq!(error, VoiceError::Connection);
        assert!(!error.to_string().contains("secret-path"));
    }
}
