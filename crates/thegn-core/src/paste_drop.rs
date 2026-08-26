//! Pure policy for the clipboard image-paste drop (THE-24).
//!
//! When the user explicitly pastes a clipboard image, thegn drops it as a
//! generated-name PNG — locally, or streamed over the pane worktree's existing
//! ssh control channel for a remote pane — and pastes the file's path. This
//! module owns the *pure* decisions: the generated filename, the size gate, the
//! confined remote drop-dir shell expression, the stale-sweep predicate, and the
//! one-shot remote shell script. Substrate-free and unit-tested under the 95%
//! gate; the host (`handlers/paste_image.rs`) does the actual clipboard read,
//! file write, and ssh stream off the event loop.
//!
//! Security notes that live here because the shape is decided here:
//! - filenames are **always** thegn-generated (`img-<utc-ms>-<token>.png`) — a
//!   clipboard's suggested name / source app is untrusted and never used;
//! - the remote drop dir is expanded so a leading `~` becomes the remote
//!   `$HOME` while the rest is single-quoted, so a dir carrying shell
//!   metacharacters cannot break out of the intended path;
//! - the sweep only ever matches `img-*.png` at `maxdepth 1`, so a stray file in
//!   the drop dir is never touched.

use crate::util;

/// The single interchange extension — every platform's clipboard read yields
/// PNG (see `clipboard::image_read_candidates`).
pub const IMAGE_EXT: &str = "png";

/// The generated-name prefix; also the sweep glob stem, so the sweep only ever
/// deletes files this feature created.
const NAME_PREFIX: &str = "img-";

/// The generated drop filename: `img-<utc-ms>-<token>.png`.
///
/// `token` is a short random alphanumeric string the caller supplies — the RNG
/// and the clock live in the host so this stays pure and testable. The name is
/// never derived from clipboard metadata.
pub fn generated_name(now_ms: u64, token: &str) -> String {
    format!("{NAME_PREFIX}{now_ms}-{token}.{IMAGE_EXT}")
}

/// The glob the sweep matches (`img-*.png`). Confines the delete to files this
/// feature wrote, so a user file that happens to share the drop dir survives.
pub fn sweep_glob() -> &'static str {
    "img-*.png"
}

/// Whether a clipboard image of `size` bytes exceeds the `[clipboard]
/// max_image_bytes` cap — checked **before** any byte is written locally or
/// leaves the machine.
pub fn over_limit(size: u64, max: u64) -> bool {
    size > max
}

/// The sweep window in minutes, for the remote `find -mmin +N`.
pub fn keep_minutes(keep_hours: u64) -> u64 {
    keep_hours.saturating_mul(60)
}

/// Whether a local drop file of the given age (seconds) is past the keep window
/// and should be swept on this paste.
pub fn sweep_eligible(age_secs: u64, keep_hours: u64) -> bool {
    age_secs >= keep_hours.saturating_mul(3600)
}

/// A POSIX-shell expression for the remote drop dir that expands a leading `~`
/// via the remote `$HOME` while single-quoting the rest.
///
/// `~/.cache/thegn/paste` → `"$HOME"/'.cache/thegn/paste'`; an absolute or
/// relative literal is single-quoted whole. The remainder is never expanded by
/// the remote shell, so a dir with spaces, `$(…)`, backticks or `;` cannot break
/// out of the intended path.
pub fn remote_dir_expr(dir: &str) -> String {
    let dir = dir.trim();
    if dir == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = dir.strip_prefix("~/") {
        format!("\"$HOME\"/{}", util::sh_quote(rest))
    } else {
        util::sh_quote(dir)
    }
}

/// The one-shot remote shell script fed to `GitLoc::sh_command` with the PNG
/// bytes on stdin. It:
/// 1. creates the confined drop dir (`mkdir -p` + `chmod 700`),
/// 2. sweeps stale `img-*.png` files older than `keep_hours` (delete confined to
///    the dir, `maxdepth 1`, regular files only),
/// 3. writes stdin to the generated-name file under `umask 077` (⇒ 0600), and
/// 4. prints the resolved absolute path on stdout (so the host learns the exact
///    remote path to paste, without a second round-trip to resolve `$HOME`).
///
/// `name` is thegn-generated (safe charset), but it is shell-quoted regardless.
pub fn remote_drop_script(dir: &str, name: &str, keep_hours: u64) -> String {
    let dir_expr = remote_dir_expr(dir);
    let name_q = util::sh_quote(name);
    let glob_q = util::sh_quote(sweep_glob());
    let mins = keep_minutes(keep_hours);
    // `d=<expr>` resolves once; every later use is the quoted `"$d"`.
    format!(
        "d={dir_expr}; mkdir -p \"$d\" && chmod 700 \"$d\" && \
         find \"$d\" -maxdepth 1 -type f -name {glob_q} -mmin +{mins} -delete 2>/dev/null; \
         umask 077 && cat > \"$d\"/{name_q} && printf '%s\\n' \"$d\"/{name_q}"
    )
}

/// A human-readable byte size for status messages (`10.0 MiB`, `512 KiB`, `3 B`).
/// Kept here so the over-limit message renders the same size the gate compared.
pub fn human_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let f = n as f64;
    if f >= MIB {
        format!("{:.1} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.0} KiB", f / KIB)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_name_is_img_ms_token_png() {
        assert_eq!(
            generated_name(1_700_000_000_123, "a1b2c3"),
            "img-1700000000123-a1b2c3.png"
        );
        // The stem matches the sweep glob so the sweep only touches our files.
        assert!(generated_name(0, "z").starts_with(NAME_PREFIX));
        assert!(generated_name(0, "z").ends_with(".png"));
    }

    #[test]
    fn over_limit_is_strict_greater_than() {
        assert!(!over_limit(10, 10), "equal to the cap is allowed");
        assert!(over_limit(11, 10));
        assert!(!over_limit(0, 10));
    }

    #[test]
    fn sweep_window_conversions() {
        assert_eq!(keep_minutes(24), 1440);
        assert_eq!(keep_minutes(0), 0);
        // saturating: an absurd keep_hours can't overflow the minute math.
        assert_eq!(keep_minutes(u64::MAX), u64::MAX);
        assert!(
            sweep_eligible(24 * 3600, 24),
            "exactly the window is eligible"
        );
        assert!(sweep_eligible(24 * 3600 + 1, 24));
        assert!(!sweep_eligible(3600, 24));
    }

    #[test]
    fn remote_dir_expr_expands_tilde_and_quotes_the_rest() {
        assert_eq!(
            remote_dir_expr("~/.cache/thegn/paste"),
            "\"$HOME\"/.cache/thegn/paste"
        );
        assert_eq!(remote_dir_expr("~"), "\"$HOME\"");
        assert_eq!(remote_dir_expr("/data/paste"), "/data/paste");
        // A dir with a space or a shell metacharacter is single-quoted whole —
        // no expansion, no break-out.
        assert_eq!(remote_dir_expr("~/my paste"), "\"$HOME\"/'my paste'");
        assert_eq!(remote_dir_expr("/tmp/$(rm -rf x)"), "'/tmp/$(rm -rf x)'");
    }

    #[test]
    fn remote_script_confines_the_sweep_and_prints_the_path() {
        let s = remote_drop_script("~/.cache/thegn/paste", "img-1-ab.png", 24);
        assert!(s.contains("mkdir -p \"$d\""));
        assert!(s.contains("chmod 700 \"$d\""));
        assert!(s.contains("umask 077"));
        assert!(s.contains("cat > \"$d\"/img-1-ab.png"));
        // Sweep is confined: maxdepth 1, regular files, the img-*.png glob only.
        assert!(s.contains("-maxdepth 1 -type f -name 'img-*.png' -mmin +1440 -delete"));
        // The path is echoed so the host can paste the resolved absolute path.
        assert!(s.contains("printf '%s\\n' \"$d\"/img-1-ab.png"));
        // No literal `~` survives into the remote command (it would not expand
        // inside the quoted `$d`).
        assert!(!s.contains('~'));
    }

    #[test]
    fn remote_script_quotes_a_hostile_name() {
        // Defense in depth: even though names are thegn-generated, a crafted one
        // is shell-quoted, never interpolated.
        let s = remote_drop_script("/d", "a b;rm.png", 1);
        assert!(s.contains("cat > \"$d\"/'a b;rm.png'"));
    }

    #[test]
    fn human_bytes_bands() {
        assert_eq!(human_bytes(3), "3 B");
        assert_eq!(human_bytes(2048), "2 KiB");
        assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MiB");
    }
}
