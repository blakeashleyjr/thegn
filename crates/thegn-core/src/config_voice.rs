//! The `[voice]` configuration family.
//!
//! Voice is deliberately a small, command-backed seam.  The commands are
//! argv arrays rather than shell strings so the host can execute them without
//! introducing a shell-quoting or vendor-specific contract.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::{config_enum, config_warn};

config_enum! {
    /// The voice provider implementation.
    pub enum VoiceKind: "voice provider" {
        Command = "command",
    } default = Command;
}

/// `[voice]` — opt-in, experimental speech-to-text through external commands.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct VoiceConfig {
    /// Explicit consent and feature switch.  When false, no worker or child
    /// process is started.
    pub enabled: bool,
    /// Provider kind.  Only the generic command provider exists in this slice.
    pub kind: VoiceKind,
    /// argv for a process that writes one complete 16-bit PCM WAV to stdout.
    pub capture_command: Vec<String>,
    /// argv for a process that reads one WAV from stdin and writes UTF-8 text.
    pub command: Vec<String>,
    /// Maximum capture length.  The effective value is clamped to a finite,
    /// bounded range before the host starts a worker.
    pub max_seconds: u64,
}

pub const MIN_MAX_SECONDS: u64 = 1;
pub const MAX_MAX_SECONDS: u64 = 300;

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            kind: VoiceKind::Command,
            capture_command: Vec::new(),
            command: Vec::new(),
            max_seconds: 30,
        }
    }
}

impl VoiceConfig {
    /// The safe capture limit used by runtime code.
    pub fn effective_max_seconds(&self) -> u64 {
        self.max_seconds.clamp(MIN_MAX_SECONDS, MAX_MAX_SECONDS)
    }

    /// The safe capture limit as a duration.
    pub fn max_duration(&self) -> Duration {
        Duration::from_secs(self.effective_max_seconds())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_disabled_and_unconfigured() {
        let cfg = VoiceConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.kind, VoiceKind::Command);
        assert!(cfg.capture_command.is_empty());
        assert!(cfg.command.is_empty());
        assert_eq!(cfg.max_seconds, 30);
    }

    #[test]
    fn max_seconds_is_bounded() {
        let mut cfg = VoiceConfig {
            max_seconds: 0,
            ..Default::default()
        };
        assert_eq!(cfg.effective_max_seconds(), MIN_MAX_SECONDS);
        cfg.max_seconds = u64::MAX;
        assert_eq!(cfg.effective_max_seconds(), MAX_MAX_SECONDS);
    }

    #[test]
    fn command_contract_round_trips() {
        let cfg: VoiceConfig = toml::from_str(
            r#"
enabled = true
kind = "command"
capture_command = ["mic", "--wav"]
command = ["stt", "--stdin"]
max_seconds = 17
"#,
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.capture_command, ["mic", "--wav"]);
        assert_eq!(cfg.command, ["stt", "--stdin"]);
        assert_eq!(cfg.max_seconds, 17);
    }
}
