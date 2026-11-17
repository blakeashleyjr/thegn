//! The MCP caller label (`THEGN_CALLER`, THE-2172, THE-2184).
//!
//! Audit label only; self-asserted; never an authorization input. It is
//! environment the caller controls, logged beside every mutating MCP tool call
//! so an operator can tell which agent asked. Nothing may branch on it to grant
//! or refuse anything: scopes come from the token, never from this label.
//!
//! The daemon sets `THEGN_CALLER=agent:<name>` for the agents it launches; the
//! Lead sets its own. UI-launched agents carry no label.

/// The environment variable the label is read from.
pub const CALLER_ENV: &str = "THEGN_CALLER";

/// The longest label kept (bytes).
pub const MAX_CALLER: usize = 64;

/// What an unusable label is replaced with.
pub const INVALID: &str = "invalid";

/// The caller label from the environment, sanitized; `None` when unset.
pub fn caller_from_env() -> Option<String> {
    std::env::var_os(CALLER_ENV).map(|raw| sanitize(&raw.to_string_lossy()))
}

/// Keep a label that is non-empty, at most [`MAX_CALLER`] bytes and drawn from
/// `[A-Za-z0-9._:/@-]`; anything else becomes [`INVALID`] rather than being
/// trimmed, so a log line never shows a forged-looking partial label.
pub fn sanitize(raw: &str) -> String {
    let ok = !raw.is_empty()
        && raw.len() <= MAX_CALLER
        && raw.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        });
    if ok {
        raw.to_string()
    } else {
        INVALID.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{INVALID, MAX_CALLER, sanitize};

    #[test]
    fn a_plain_label_is_kept() {
        assert_eq!(sanitize("agent:pipeline-coder"), "agent:pipeline-coder");
        assert_eq!(sanitize("lead/tab.2@host_1"), "lead/tab.2@host_1");
        assert_eq!(sanitize(&"a".repeat(MAX_CALLER)), "a".repeat(MAX_CALLER));
    }

    #[test]
    fn empty_is_invalid() {
        assert_eq!(sanitize(""), INVALID);
    }

    #[test]
    fn too_long_is_invalid() {
        assert_eq!(sanitize(&"a".repeat(MAX_CALLER + 1)), INVALID);
    }

    #[test]
    fn bad_characters_are_invalid() {
        for raw in [
            "agent x",
            "agent\nx",
            "agent;rm",
            "a$b",
            "a\u{1b}[31m",
            "a\"b",
        ] {
            assert_eq!(sanitize(raw), INVALID, "{raw:?}");
        }
    }

    #[test]
    fn unicode_is_invalid() {
        assert_eq!(sanitize("agent:\u{e9}"), INVALID);
        assert_eq!(sanitize("\u{430}gent"), INVALID); // Cyrillic a
        assert_eq!(sanitize("agent\u{200b}"), INVALID);
    }
}
