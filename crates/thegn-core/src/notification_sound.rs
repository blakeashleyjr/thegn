//! Pure sound-reference vocabulary used by notification routing.
//!
//! The host owns path expansion, pack inspection, players, and playback. Core
//! only distinguishes the explicit references a trusted configuration may ask
//! for; in particular, a per-kind value can never become a shell command.

/// A sound selected by notification policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundRef {
    /// Do not emit an audible cue.
    Off,
    /// Emit the terminal bell.
    Bell,
    /// Look up this name in the configured trusted pack.
    Pack(String),
    /// Play this explicit user-provided path.
    File(String),
}

impl SoundRef {
    /// Parse the restricted per-kind sound vocabulary.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let value = raw.trim();
        if value.is_empty() {
            return Err("sound reference is empty".into());
        }
        match value.to_ascii_lowercase().as_str() {
            "off" | "none" => return Ok(Self::Off),
            "bell" | "terminal" | "builtin:bell" => return Ok(Self::Bell),
            _ => {}
        }

        if let Some(name) = value.strip_prefix("pack:") {
            let name = name.trim();
            if name.is_empty() {
                return Err("pack sound name is empty after pack:".into());
            }
            if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
                return Err(format!("invalid pack sound name {name:?}"));
            }
            if name.chars().any(|c| c.is_control()) {
                return Err("pack sound name contains a control character".into());
            }
            return Ok(Self::Pack(name.to_string()));
        }

        if Self::is_user_path(value) {
            return Ok(Self::File(value.to_string()));
        }

        Err(format!(
            "unsupported sound reference {value:?}; expected off, bell, builtin:bell, pack:<name>, or an absolute/~ path"
        ))
    }

    /// Whether a string is an explicit absolute or tilde-prefixed user path.
    ///
    /// This is syntax-only: checking that the path exists belongs to the host.
    pub(crate) fn is_user_path(value: &str) -> bool {
        value == "~"
            || value.starts_with("~/")
            || value.starts_with("~\\")
            || value.starts_with('/')
            || (value.len() >= 3
                && value.as_bytes()[0].is_ascii_alphabetic()
                && value.as_bytes()[1] == b':'
                && matches!(value.as_bytes()[2], b'/' | b'\\'))
            || value.starts_with("\\\\")
    }

    pub fn pack_name(&self) -> Option<&str> {
        match self {
            Self::Pack(name) => Some(name),
            _ => None,
        }
    }

    pub fn file_path(&self) -> Option<&str> {
        match self {
            Self::File(path) => Some(path),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SoundRef;

    #[test]
    fn parser_accepts_aliases_and_explicit_references() {
        assert_eq!(SoundRef::parse("off"), Ok(SoundRef::Off));
        assert_eq!(SoundRef::parse("NONE"), Ok(SoundRef::Off));
        assert_eq!(SoundRef::parse("terminal"), Ok(SoundRef::Bell));
        assert_eq!(SoundRef::parse("builtin:bell"), Ok(SoundRef::Bell));
        assert_eq!(
            SoundRef::parse("pack:attention"),
            Ok(SoundRef::Pack("attention".into()))
        );
        assert_eq!(
            SoundRef::parse("~/sounds/done.wav"),
            Ok(SoundRef::File("~/sounds/done.wav".into()))
        );
        assert_eq!(
            SoundRef::parse("/tmp/done.wav"),
            Ok(SoundRef::File("/tmp/done.wav".into()))
        );
    }

    #[test]
    fn parser_rejects_commands_relative_paths_and_bad_pack_names() {
        for value in [
            "",
            "   ",
            "done.wav",
            "paplay /tmp/done.wav",
            "pack:",
            "pack:../escape",
            "pack:sub/name",
            "builtin:chime",
            "~user/sound.wav",
        ] {
            assert!(SoundRef::parse(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn user_path_syntax_covers_windows_absolute_paths() {
        assert!(SoundRef::parse(r"C:\sounds\done.wav").is_ok());
        assert!(SoundRef::parse(r"\\server\share\done.wav").is_ok());
    }
}
