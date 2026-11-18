//! Sprites keep-awake holds: the pure policy shared by the host bridge
//! (`thegn sprite-exec --hold`) and the svc Tasks-API client.
//!
//! A headless command on a sprite is wrapped in a POSIX `sh` heartbeat that
//! registers a task named `thegn-hold-<16 hex>` with a [`HOLD_EXPIRE_SECS`]
//! expiry, refreshes it every [`HOLD_REFRESH_SECS`] while the command runs, and
//! stops refreshing at a ceiling. Interactive panes get no hold.
//!
//! Spine stub (b160 lane 1): the constants are final; the bodies below are the
//! minimal versions lane 3 replaces.

/// Expiry of one hold registration: worst-case spend after a crash is the
/// ceiling plus this.
pub const HOLD_EXPIRE_SECS: u64 = 300;
/// How often the in-sprite heartbeat refreshes the hold.
pub const HOLD_REFRESH_SECS: u64 = 60;
/// The ceiling when `[env.<n>.provider] max_lifetime_secs` is unset.
pub const HOLD_DEFAULT_CEILING_SECS: u64 = 4 * 3600;
/// The largest ceiling `--hold` may request.
pub const HOLD_MAX_CEILING_SECS: u64 = 24 * 3600;
/// Host-side only: the hold duration `sprite-exec` reads from its environment.
pub const HOLD_ENV: &str = "THEGN_SPRITE_HOLD_SECS";

/// The task name for a hold nonce: `thegn-hold-<16 lowercase hex>`. `None` when
/// the nonce is not exactly 16 lowercase hex digits.
pub fn hold_name(nonce_hex: &str) -> Option<String> {
    let valid = nonce_hex.len() == 16
        && nonce_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    valid.then(|| format!("thegn-hold-{nonce_hex}"))
}

/// The hold ceiling: an explicit request (capped at [`HOLD_MAX_CEILING_SECS`]),
/// else `max_lifetime_secs` when set, else [`HOLD_DEFAULT_CEILING_SECS`].
pub fn ceiling_secs(requested: Option<u64>, max_lifetime_secs: u64) -> u64 {
    match requested {
        Some(secs) => secs.min(HOLD_MAX_CEILING_SECS),
        None if max_lifetime_secs > 0 => max_lifetime_secs,
        None => HOLD_DEFAULT_CEILING_SECS,
    }
}

/// Wrap `argv` in the heartbeat script:
/// `["/bin/sh","-c",SCRIPT,"thegn-hold",task,ceiling,argv...]`.
///
/// Spine stub: returns `argv` unchanged (today's unheld behaviour) until lane 3
/// lands the script.
pub fn wrap_argv(argv: &[String], task: &str, ceiling_secs: u64) -> Vec<String> {
    tracing::debug!(
        target: "thegn::sprites",
        task,
        ceiling_secs,
        "sprite hold: wrapper not built yet; running unheld"
    );
    argv.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_name_accepts_only_sixteen_lowercase_hex() {
        assert_eq!(
            hold_name("0123456789abcdef").as_deref(),
            Some("thegn-hold-0123456789abcdef")
        );
        assert_eq!(hold_name("0123456789ABCDEF"), None);
        assert_eq!(hold_name("0123"), None);
        assert_eq!(hold_name("0123456789abcdeg"), None);
    }

    #[test]
    fn ceiling_prefers_request_then_lifetime_then_default() {
        assert_eq!(ceiling_secs(Some(60), 10), 60);
        assert_eq!(ceiling_secs(Some(u64::MAX), 0), HOLD_MAX_CEILING_SECS);
        assert_eq!(ceiling_secs(None, 900), 900);
        assert_eq!(ceiling_secs(None, 0), HOLD_DEFAULT_CEILING_SECS);
    }
}
