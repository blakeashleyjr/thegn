//! Explicit dictation setup and artifact management; never opens a microphone.
use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use thegn_core::config::{Config, VoiceConfig, VoiceKind};
use thegn_core::{msg, outln};
use thegn_svc::voice_models as models;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Inspect effective provider, model, native build, and channel readiness.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Select language/provider/model, confirm downloads, and save configuration.
    Setup {
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value="embedded", value_parser=["embedded","http","command"])]
        kind: String,
        #[arg(long)]
        endpoint: Option<String>,
        /// Secret reference, e.g. env:STT_TOKEN (never a token value).
        #[arg(long)]
        credential: Option<String>,
        #[arg(long)]
        model_path: Option<PathBuf>,
        /// Disable rolling requests and repeated HTTP audio billing.
        #[arg(long)]
        final_only: bool,
        /// Accept the displayed model download size without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Manage pinned Whisper GGML artifacts in the user data directory.
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
}
#[derive(clap::Subcommand, Clone)]
pub enum ModelAction {
    List {
        #[arg(long)]
        json: bool,
    },
    Pull {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    Rm {
        name: String,
    },
}

/// Inputs to `readiness`, separated from the process so every branch is testable.
struct Facts<'a> {
    channel_allowed: bool,
    native_compiled: bool,
    model_present: bool,
    model_error: Option<&'a str>,
}

/// One-line verdict: `None` when dictation can start, else the next step a user
/// should take. Never opens the microphone, so it cannot prove device access.
fn readiness(cfg: &VoiceConfig, facts: &Facts) -> Option<String> {
    if !facts.channel_allowed {
        return Some(
            "voice is experimental and off on the stable channel; relaunch with THEGN_CHANNEL=dev"
                .into(),
        );
    }
    if !cfg.enabled {
        return Some("voice is disabled; run `thegn voice setup`".into());
    }
    let native_capture = cfg.capture_command.is_empty() || cfg.kind == VoiceKind::Embedded;
    if native_capture && !facts.native_compiled {
        return Some(
            "native voice is not built into this binary (musl/Windows builds omit it); set [voice] capture_command or use a glibc Linux or macOS build"
                .into(),
        );
    }
    match cfg.kind {
        VoiceKind::Embedded => {
            if let Some(error) = facts.model_error {
                return Some(format!(
                    "model unresolved ({error}); run `thegn voice setup`"
                ));
            }
            if !facts.model_present {
                return Some(format!(
                    "model `{}` is not installed; run `thegn voice model pull {}`",
                    cfg.model, cfg.model
                ));
            }
        }
        VoiceKind::Http if cfg.endpoint.is_empty() => {
            return Some("no endpoint; run `thegn voice setup --kind http --endpoint URL`".into());
        }
        VoiceKind::Command if cfg.command.is_empty() => {
            return Some("no [voice] command configured for kind = \"command\"".into());
        }
        _ => {}
    }
    None
}

pub fn status(cfg: &VoiceConfig) -> serde_json::Value {
    let (path, model_error) = if cfg.kind == VoiceKind::Embedded {
        match models::resolve(cfg) {
            Ok(path) => (Some(path), None),
            Err(error) => (None, Some(error.to_string())),
        }
    } else {
        (None, None)
    };
    let native_capture = cfg.capture_command.is_empty() || cfg.kind == VoiceKind::Embedded;
    let model_present = path.as_ref().is_some_and(|p| p.is_file());
    let channel_allowed = crate::channel_state::allows(thegn_core::channel::Feature::Voice);
    let next_step = readiness(
        cfg,
        &Facts {
            channel_allowed,
            native_compiled: cfg!(voice_native),
            model_present,
            model_error: model_error.as_deref(),
        },
    );
    serde_json::json!({
        "channel_allowed": channel_allowed,
        "ready": next_step.is_none(),
        "next_step": next_step,
        "enabled": cfg.enabled, "kind": cfg.kind.as_str(), "language": cfg.language,
        "model": cfg.model, "model_path": path.as_ref().map(|p|p.display().to_string()),
        "model_error": model_error,
        "model_present": model_present, "native_compiled": cfg!(voice_native),
        "metal_compiled": cfg!(voice_metal), "live_preview": cfg.live_preview && cfg.kind != VoiceKind::Command && native_capture && cfg!(voice_native),
        "credential_configured": cfg.credential.is_some(), "timeout_seconds": cfg.request_timeout().as_secs(),
        "setup": "thegn voice setup; ready means configuration is complete, not that the microphone works (macOS: allow Microphone for your terminal)"
    })
}

fn validate_language(language: &str) -> Result<()> {
    anyhow::ensure!(
        !language.is_empty()
            && language.len() <= 32
            && language
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '-'),
        "use a language code or auto"
    );
    Ok(())
}

fn validate_credential_reference(reference: &str) -> Result<()> {
    anyhow::ensure!(
        ["env:", "file:", "keyring:"]
            .iter()
            .any(|prefix| reference.starts_with(prefix)),
        "credential must be an env:, file:, or keyring: reference"
    );
    Ok(())
}

fn remove_model(dir: &std::path::Path, name: &str) -> Result<()> {
    models::remove(dir, name)
}

pub fn run(cfg: &Config, action: Action, config_path: PathBuf) -> Result<()> {
    match action {
        Action::Status { json } => {
            let report = status(&cfg.voice);
            if json {
                super::emit_json(&report)?;
            } else {
                outln!(
                    "Voice: {} ({})",
                    if cfg.voice.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    cfg.voice.kind.as_str()
                );
                outln!(
                    "Language: {}; model: {}; native build: {}; live preview: {}",
                    cfg.voice.language,
                    cfg.voice.model,
                    report["native_compiled"],
                    report["live_preview"]
                );
                outln!(
                    "Ready: {}",
                    match report["next_step"].as_str() {
                        None => "yes (press Alt v in a pane)".to_string(),
                        Some(step) => format!("no - {step}"),
                    }
                );
                outln!("{}", report["setup"].as_str().unwrap_or_default());
            }
            Ok(())
        }
        Action::Model { action } => match action {
            ModelAction::List { json } => {
                let rows: Vec<_> = models::MODELS.iter().map(|m| serde_json::json!({ "name": m.name,"bytes":m.bytes,"sha256":m.sha256,"installed":models::path_in(&models::store_dir(),m.name).is_ok_and(|p|p.is_file()) })).collect();
                if json {
                    super::emit_json(&rows)?;
                } else {
                    for row in rows {
                        outln!(
                            "{}\t{} bytes\t{}",
                            row["name"].as_str().unwrap_or_default(),
                            row["bytes"],
                            if row["installed"] == true {
                                "installed"
                            } else {
                                "absent"
                            }
                        );
                    }
                }
                Ok(())
            }
            ModelAction::Pull { name, yes } => {
                pull(&name, yes)?;
                Ok(())
            }
            ModelAction::Rm { name } => remove_model(&models::store_dir(), &name),
        },
        Action::Setup {
            language,
            model,
            kind,
            endpoint,
            credential,
            model_path,
            final_only,
            yes,
        } => {
            let language = match language {
                Some(v) => v,
                None => prompt("Language code (en, fr, …; auto for detection)", "en")?,
            };
            validate_language(&language)?;
            let recommended = if language == "en" { "base.en" } else { "base" };
            let selected = match model {
                Some(v) => v,
                None if yes || kind != "embedded" => recommended.into(),
                None => prompt(
                    "Whisper model (tiny/base/small; .en for English)",
                    recommended,
                )?,
            };
            let provider = match kind.as_str() {
                "embedded" => VoiceKind::Embedded,
                "http" => VoiceKind::Http,
                _ => VoiceKind::Command,
            };
            let mut voice = cfg.voice.clone();
            voice.enabled = true;
            voice.kind = provider;
            voice.language = language;
            voice.model = selected;
            voice.model_path = model_path.map(|p| p.to_string_lossy().into_owned());
            voice.live_preview = !final_only;
            if let Some(endpoint) = endpoint {
                voice.endpoint = endpoint;
            }
            if let Some(reference) = credential {
                validate_credential_reference(&reference)?;
                voice.credential = Some(reference);
            }
            match provider {
                VoiceKind::Embedded => {
                    anyhow::ensure!(
                        cfg!(voice_native),
                        "native voice is not built into this binary (musl/Windows builds omit it); use a glibc Linux or macOS build"
                    );
                    #[cfg(voice_native)]
                    thegn_voice::whisper::Engine::validate_language(&voice.language)?;
                    let path = models::resolve(&voice)?;
                    if voice.model_path.is_some() {
                        anyhow::ensure!(path.is_file(), "local model path does not exist");
                    } else if !path.is_file() {
                        pull(&voice.model, yes)?;
                    } else {
                        models::verify(
                            &path,
                            models::model(&voice.model)?,
                            &AtomicBool::new(false),
                        )?;
                    }
                }
                VoiceKind::Http => {
                    if voice.endpoint.is_empty() {
                        voice.endpoint = prompt(
                            "Full HTTP transcription endpoint",
                            "http://127.0.0.1:8000/v1/audio/transcriptions",
                        )?;
                    }
                    thegn_svc::voice_http::validate_endpoint(&voice.endpoint)?;
                    voice
                        .validate_credential_transport()
                        .map_err(anyhow::Error::msg)?;
                    if voice.live_preview {
                        msg::warn(
                            "Live HTTP previews repeatedly send overlapping audio and may incur repeated billing. Use --final-only to send just the final recording.",
                        );
                    }
                    if !cfg!(voice_native) && voice.capture_command.is_empty() {
                        bail!("HTTP needs native capture or [voice] capture_command");
                    }
                }
                VoiceKind::Command => {
                    anyhow::ensure!(
                        !voice.command.is_empty(),
                        "configure [voice] command before setup --kind command"
                    );
                    anyhow::ensure!(
                        cfg!(voice_native) || !voice.capture_command.is_empty(),
                        "command transcription needs native capture or [voice] capture_command"
                    );
                }
            }
            thegn_core::config_write::set_voice(&config_path, &voice)?;
            outln!(
                "Voice configured. Alt v records/stops; Esc cancels. Audio is pasted without submission."
            );
            Ok(())
        }
    }
}
#[expect(
    clippy::disallowed_macros,
    reason = "explicit interactive stderr prompt"
)]
fn prompt(label: &str, default: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("{label}: pass the explicit option in noninteractive mode");
    }
    eprint!("{label} [{default}]: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer)? == 0 {
        bail!("voice setup cancelled");
    }
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        default.into()
    } else {
        answer.into()
    })
}
#[expect(
    clippy::disallowed_macros,
    reason = "explicit interactive download progress on stderr"
)]
fn pull(name: &str, yes: bool) -> Result<PathBuf> {
    let artifact = models::model(name)?;
    eprintln!(
        "Whisper {}: {} bytes ({:.1} MiB), pinned SHA-256; verified after download.",
        name,
        artifact.bytes,
        artifact.bytes as f64 / 1048576.0
    );
    if !yes && !prompt("Download? Type yes to continue", "no")?.eq_ignore_ascii_case("yes") {
        bail!("model download cancelled");
    }
    let cancel = AtomicBool::new(false);
    let mut last = std::time::Instant::now();
    let path = models::pull(&models::store_dir(), artifact, &cancel, |done, total| {
        if last.elapsed() >= std::time::Duration::from_millis(250) || done == total {
            eprint!("\rDownloading: {done}/{total} bytes");
            last = std::time::Instant::now();
        }
    })
    .context("model download failed; run voice model pull again to retry")?;
    eprintln!("\nInstalled {}", path.display());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_arguments_validate_language_credential_reference_and_endpoint() {
        assert!(validate_language("auto").is_ok());
        assert!(validate_language("en-US").is_ok());
        assert!(validate_language("bad/locale").is_err());
        assert!(validate_credential_reference("env:STT_TOKEN").is_ok());
        assert!(validate_credential_reference("file:/tmp/token").is_ok());
        assert!(validate_credential_reference("literal-token").is_err());
        assert!(thegn_svc::voice_http::validate_endpoint("http://127.0.0.1:8000/stt").is_ok());
        assert!(thegn_svc::voice_http::validate_endpoint("file:///tmp/audio").is_err());
    }

    #[test]
    fn status_json_has_the_documented_readiness_shape() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let report = status(&VoiceConfig::default());
        for key in [
            "enabled",
            "kind",
            "language",
            "model",
            "model_path",
            "model_error",
            "model_present",
            "native_compiled",
            "metal_compiled",
            "live_preview",
            "credential_configured",
            "timeout_seconds",
            "setup",
            "ready",
            "next_step",
            "channel_allowed",
        ] {
            assert!(report.get(key).is_some(), "missing status key {key}");
        }
        assert_eq!(report["enabled"], false);
        assert_eq!(report["kind"], "command");
        assert_eq!(report["ready"], false);
        assert!(report["next_step"].is_string());
    }

    fn facts(channel_allowed: bool, native: bool, model: bool) -> Facts<'static> {
        Facts {
            channel_allowed,
            native_compiled: native,
            model_present: model,
            model_error: None,
        }
    }

    #[test]
    fn readiness_names_the_next_step_in_order() {
        let mut cfg = VoiceConfig {
            kind: VoiceKind::Embedded,
            ..VoiceConfig::default()
        };
        let stable = readiness(&cfg, &facts(false, true, true)).unwrap();
        assert!(stable.contains("THEGN_CHANNEL=dev"));
        assert!(
            readiness(&cfg, &facts(true, true, true))
                .unwrap()
                .contains("thegn voice setup")
        );
        cfg.enabled = true;
        assert!(
            readiness(&cfg, &facts(true, false, true))
                .unwrap()
                .contains("no native capture")
        );
        let pull = readiness(&cfg, &facts(true, true, false)).unwrap();
        assert!(pull.contains(&format!("voice model pull {}", cfg.model)));
        assert_eq!(readiness(&cfg, &facts(true, true, true)), None);
    }

    #[test]
    fn readiness_checks_http_and_command_requirements() {
        let mut cfg = VoiceConfig {
            enabled: true,
            kind: VoiceKind::Http,
            ..VoiceConfig::default()
        };
        cfg.endpoint.clear();
        assert!(
            readiness(&cfg, &facts(true, true, false))
                .unwrap()
                .contains("no endpoint")
        );
        cfg.endpoint = "http://127.0.0.1:8000/v1/audio/transcriptions".into();
        assert_eq!(readiness(&cfg, &facts(true, true, false)), None);
        cfg.kind = VoiceKind::Command;
        cfg.command.clear();
        cfg.capture_command = vec!["rec".into()];
        assert!(
            readiness(&cfg, &facts(true, false, false))
                .unwrap()
                .contains("no [voice] command")
        );
    }

    #[test]
    fn model_remove_is_idempotent() {
        let state = thegn_core::testenv::IsolatedState::new();
        let dir = state.root().join("models");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ggml-tiny.bin"), b"model").unwrap();
        std::fs::write(dir.join("ggml-tiny.verified.json"), b"marker").unwrap();
        let stale = dir.join(".thegn-voice-model-stale");
        std::fs::write(&stale, b"temp").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 60 + 1);
        std::fs::File::open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        assert!(remove_model(&dir, "tiny").is_ok());
        assert!(!dir.join("ggml-tiny.bin").exists());
        assert!(!dir.join("ggml-tiny.verified.json").exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert!(remove_model(&dir, "tiny").is_ok());
    }
}
