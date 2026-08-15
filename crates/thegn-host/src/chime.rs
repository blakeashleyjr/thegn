//! Audible "chime" cue for notifications (`[notifications.sound] mode = "chime"`).
//!
//! Core's routing engine resolves a decision to
//! [`thegn_core::notification_route::SoundEmit::Chime`]; the host turns that into
//! real audio here. It plays a sound file through the first system audio player
//! it can find. When `chime_file` is unset it materializes a bundled chime WAV
//! (synthesized once, so there is no binary asset to ship) under the state dir.
//!
//! Everything is best-effort and off the event loop: player detection is cached,
//! the file is written once, and playback is spawned on a detached thread (via
//! [`crate::notify::spawn_sound_command`]). If no player is available, [`play`]
//! returns `false` so the caller can fall back to the terminal bell — a chime
//! must never silently no-op.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Play the chime. `chime_file` is the configured custom sound (empty ⇒ the
/// bundled chime). Returns `false` when no player/file is available so the
/// caller can ring the terminal bell instead.
pub fn play(chime_file: &str) -> bool {
    let Some(file) = resolve_file(chime_file) else {
        return false;
    };
    let Some(cmd) = play_command(&file) else {
        return false;
    };
    crate::notify::spawn_sound_command(&cmd);
    true
}

/// Resolve the sound file: a user `chime_file` (tilde-expanded) if set and
/// present, else the materialized bundled chime.
fn resolve_file(chime_file: &str) -> Option<PathBuf> {
    let trimmed = chime_file.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(thegn_core::util::expand_tilde(trimmed));
        // A missing custom file falls back to the bundled chime rather than
        // handing the player a bad path (which would just fail silently).
        if p.exists() {
            return Some(p);
        }
    }
    materialized_bundle()
}

/// The bundled chime file under the state dir, written on first use.
fn materialized_bundle() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = thegn_core::util::xdg_state_home().join("thegn");
        let path = dir.join("chime.wav");
        if path.exists() {
            return Some(path);
        }
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        match std::fs::write(&path, synth_chime_wav()) {
            Ok(()) => Some(path),
            Err(_) => None,
        }
    })
    .clone()
}

/// Build the play command for the first available system player. Cached: the
/// PATH lookups happen once. The `<file>` is shell-quoted for the `sh -c` /
/// PowerShell command line.
fn play_command(file: &std::path::Path) -> Option<String> {
    static PLAYER: OnceLock<Option<Player>> = OnceLock::new();
    let player = PLAYER.get_or_init(detect_player).as_ref()?;
    Some(player.command(file))
}

/// A detected audio player and how to invoke it.
struct Player {
    /// Program name (looked up on PATH).
    prog: &'static str,
    /// Extra args before the file path.
    args: &'static [&'static str],
    /// Windows plays via a PowerShell one-liner rather than a plain program.
    powershell: bool,
}

impl Player {
    fn command(&self, file: &std::path::Path) -> String {
        let quoted = shell_quote(&file.to_string_lossy());
        if self.powershell {
            // `SoundPlayer.PlaySync()` blocks until the clip finishes; the whole
            // command already runs on a detached thread.
            format!(
                "powershell -NoProfile -Command \"(New-Object Media.SoundPlayer {quoted}).PlaySync()\""
            )
        } else if self.args.is_empty() {
            format!("{} {quoted}", self.prog)
        } else {
            format!("{} {} {quoted}", self.prog, self.args.join(" "))
        }
    }
}

/// Detect the first available system player. Order favors low-latency,
/// commonly-present players per platform. WAV is accepted by all of them.
fn detect_player() -> Option<Player> {
    #[cfg(target_os = "macos")]
    {
        if have("afplay") {
            return Some(Player {
                prog: "afplay",
                args: &[],
                powershell: false,
            });
        }
    }
    #[cfg(windows)]
    {
        // PowerShell ships with Windows; no PATH probe needed.
        return Some(Player {
            prog: "powershell",
            args: &[],
            powershell: true,
        });
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // Linux / other unix: try the usual suspects in order.
        for (prog, args) in [
            ("pw-play", &[][..]),
            ("paplay", &[][..]),
            ("aplay", &["-q"][..]),
            (
                "ffplay",
                &["-nodisp", "-autoexit", "-loglevel", "quiet"][..],
            ),
            ("play", &["-q"][..]),
        ] {
            if have(prog) {
                return Some(Player {
                    prog,
                    args,
                    powershell: false,
                });
            }
        }
    }
    #[allow(unreachable_code)]
    None
}

/// Whether `prog` is on PATH. Only the macOS (`afplay`) + Linux (paplay/…)
/// player-detection paths consult it; the Windows path uses PowerShell's built-in
/// audio, so gate it out there to avoid a dead-code warning on the -gnu cross-check.
#[cfg(not(windows))]
fn have(prog: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(prog).is_file())
}

/// Single-quote a string for a POSIX `sh -c` command line (also fine as a
/// PowerShell single-quoted string). Embedded single quotes are escaped.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Synthesize a short, pleasant two-note "ding" chime as 16-bit mono PCM WAV
/// (44.1 kHz). Two ascending notes (A5 → D6) with a soft harmonic and an
/// exponential decay — warm and unobtrusive, no external asset needed.
fn synth_chime_wav() -> Vec<u8> {
    const RATE: u32 = 44_100;
    // (frequency Hz, start second, duration seconds).
    let notes: [(f32, f32, f32); 2] = [(880.0, 0.0, 0.42), (1174.66, 0.14, 0.5)];
    let total = 0.66_f32;
    let n = (RATE as f32 * total) as usize;
    let mut samples: Vec<i16> = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / RATE as f32;
        let mut v = 0.0_f32;
        for (freq, start, dur) in notes {
            if t < start || t >= start + dur {
                continue;
            }
            let local = t - start;
            // Exponential decay envelope for a bell-like fade.
            let env = (-4.0 * local / dur).exp();
            let phase = 2.0 * std::f32::consts::PI * freq * local;
            // Fundamental plus a quieter octave harmonic for warmth.
            let tone = phase.sin() + 0.25 * (2.0 * phase).sin();
            v += env * tone;
        }
        // Headroom so the harmonic sum never clips.
        let clamped = (v * 0.32).clamp(-1.0, 1.0);
        samples.push((clamped * i16::MAX as f32) as i16);
    }
    encode_wav_mono16(&samples, RATE)
}

/// Encode mono 16-bit PCM samples as a canonical 44-byte-header WAV.
fn encode_wav_mono16(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = rate * 2; // mono, 2 bytes/sample
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels = mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_well_formed() {
        let wav = synth_chime_wav();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        // Header (44) + at least some audio.
        assert!(wav.len() > 44 + 1000, "wav too short: {}", wav.len());
        // Declared data length matches the payload.
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
        assert_eq!(data_len, wav.len() - 44);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/a/b.wav"), "'/a/b.wav'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn player_command_includes_quoted_file() {
        let p = Player {
            prog: "paplay",
            args: &[],
            powershell: false,
        };
        let cmd = p.command(std::path::Path::new("/tmp/chime.wav"));
        assert_eq!(cmd, "paplay '/tmp/chime.wav'");
        let p2 = Player {
            prog: "aplay",
            args: &["-q"],
            powershell: false,
        };
        assert_eq!(
            p2.command(std::path::Path::new("/tmp/c.wav")),
            "aplay -q '/tmp/c.wav'"
        );
    }
}
