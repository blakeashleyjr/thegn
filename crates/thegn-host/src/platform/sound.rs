//! Platform sound-player seam.
//!
//! Player names, platform conditionals, and provider-specific argv belong here.
//! The notification runtime only sees an object-safe synchronous provider and
//! an immutable capability/probe report.

use std::path::Path;

use thegn_core::seam::{Availability, ProbeReport};

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SoundCaps {
    pub(crate) formats: Vec<&'static str>,
    pub(crate) volume: bool,
}

#[derive(Debug)]
pub(crate) enum SoundError {
    Spawn(std::io::Error),
    Failed,
}

impl std::fmt::Display for SoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "sound player failed to start: {e}"),
            Self::Failed => f.write_str("sound player returned a failure status"),
        }
    }
}

impl std::error::Error for SoundError {}

/// Synchronous by design: callers invoke it only from the notify-sound worker.
pub(crate) trait SoundPlayer: Send + Sync {
    fn id(&self) -> &'static str;
    fn caps(&self) -> SoundCaps;
    fn probe(&self) -> ProbeReport;
    fn play(&self, path: &Path, volume: f32) -> Result<(), SoundError>;
}

struct Player {
    id: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    formats: &'static [&'static str],
    volume: bool,
    powershell: bool,
}

impl Player {
    fn caps(&self) -> SoundCaps {
        SoundCaps {
            formats: self.formats.to_vec(),
            volume: self.volume,
        }
    }

    fn argv(&self, path: &Path, volume: f32) -> Vec<String> {
        if self.powershell {
            // The path is a separate argv item after `--`; it is never shell
            // quoted or interpolated into a command line.
            return vec![
                "-NoProfile".into(),
                "-Command".into(),
                "$p = New-Object Media.SoundPlayer $args[0]; $p.PlaySync()".into(),
                "--".into(),
                path.to_string_lossy().into_owned(),
            ];
        }
        let mut args = self
            .args
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>();
        if self.volume {
            // `pw-play` accepts a linear volume hint. Other providers leave
            // the hint untouched because they have no portable volume argv.
            args.extend(
                ["--volume", &volume.to_string()]
                    .into_iter()
                    .map(str::to_owned),
            );
        }
        args.push(path.to_string_lossy().into_owned());
        args
    }
}

impl SoundPlayer for Player {
    fn id(&self) -> &'static str {
        self.id
    }

    fn caps(&self) -> SoundCaps {
        self.caps()
    }

    fn probe(&self) -> ProbeReport {
        ProbeReport::new("sound", self.id(), Availability::Ready)
            .with_caps(&self.caps())
            .note(format!("formats: {}", self.formats.join(", ")))
            .note(format!(
                "volume: {}",
                if self.volume {
                    "supported"
                } else {
                    "unsupported"
                }
            ))
    }

    #[expect(clippy::disallowed_methods)]
    fn play(&self, path: &Path, volume: f32) -> Result<(), SoundError> {
        let mut command = std::process::Command::new(self.program);
        command.args(self.argv(path, volume));
        let status = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(SoundError::Spawn)?;
        status.success().then_some(()).ok_or(SoundError::Failed)
    }
}

#[cfg(not(windows))]
fn have(program: &str) -> bool {
    thegn_core::util::which_path(program).is_some()
}

fn detected() -> Option<Player> {
    #[cfg(target_os = "macos")]
    if have("afplay") {
        return Some(Player {
            id: "afplay",
            program: "afplay",
            args: &[],
            formats: &["wav", "aiff", "caf", "m4a"],
            volume: false,
            powershell: false,
        });
    }

    #[cfg(windows)]
    {
        return Some(Player {
            id: "powershell",
            program: "powershell",
            args: &[],
            formats: &["wav"],
            volume: false,
            powershell: true,
        });
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    for (id, program, args, formats, volume) in [
        (
            "pw-play",
            "pw-play",
            &[][..],
            &["wav", "flac", "ogg", "mp3"][..],
            true,
        ),
        (
            "paplay",
            "paplay",
            &[][..],
            &["wav", "aiff", "flac", "ogg"][..],
            false,
        ),
        ("aplay", "aplay", &["-q"][..], &["wav"][..], false),
        (
            "ffplay",
            "ffplay",
            &["-nodisp", "-autoexit", "-loglevel", "quiet"][..],
            &["wav", "aiff", "flac", "ogg", "mp3"][..],
            false,
        ),
        (
            "play",
            "play",
            &["-q"][..],
            &["wav", "aiff", "flac", "ogg", "mp3"][..],
            false,
        ),
    ] {
        if have(program) {
            return Some(Player {
                id,
                program,
                args,
                formats,
                volume,
                powershell: false,
            });
        }
    }
    None
}

pub(crate) fn provider() -> Option<Box<dyn SoundPlayer>> {
    detected().map(|p| Box::new(p) as Box<dyn SoundPlayer>)
}

pub(crate) fn probe() -> ProbeReport {
    provider().map_or_else(
        || {
            ProbeReport::new(
                "sound",
                "none",
                Availability::Unavailable("no supported audio player found".into()),
            )
            .note("fallback: terminal bell")
        },
        |p| p.probe(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_argv_keeps_paths_as_arguments() {
        let player = Player {
            id: "paplay",
            program: "paplay",
            args: &[],
            formats: &["wav"],
            volume: false,
            powershell: false,
        };
        assert_eq!(
            player.argv(Path::new("/tmp/a b.wav"), 1.0),
            ["/tmp/a b.wav"]
        );
    }

    #[test]
    fn powershell_provider_uses_a_fixed_script_and_path_argv() {
        let player = Player {
            id: "powershell",
            program: "powershell",
            args: &[],
            formats: &["wav"],
            volume: false,
            powershell: true,
        };
        let argv = player.argv(Path::new("C:\\sounds\\a b.wav"), 1.0);
        assert_eq!(argv[0], "-NoProfile");
        assert_eq!(argv[3], "--");
        assert_eq!(argv[4], r"C:\sounds\a b.wav");
        assert!(!argv[2].contains("C:\\sounds"));
    }
}
