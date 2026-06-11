//! The bottom file-manager drawer's yazi: a private, version-pinned build with a
//! private config dir, fully isolated from the user's system yazi.
//!
//! - binary: `SUPERZEJ_YAZI_BIN` (wired by Nix to the pinned `superzej-yazi`);
//! - config: `YAZI_CONFIG_HOME` = `<superzej-dir>/yazi` by default, seeded once
//!   from the bundled defaults below and never overwritten. Only the derived
//!   `theme.toml` is regenerated, from the superzej accent, so the drawer matches
//!   the palette.

use crate::config::Config;
use crate::util;
use std::path::PathBuf;

/// Bundled yazi config, embedded in the binary and seeded at first launch.
const YAZI_TOML: &str = include_str!("../../../config/yazi/yazi.toml");
const KEYMAP_TOML: &str = include_str!("../../../config/yazi/keymap.toml");
/// The managed yazi.toml block that disables image preview/preload helpers.
const IMAGE_POLICY_BEGIN: &str = "# BEGIN SUPERZEJ MANAGED IMAGE PREVIEW POLICY";
const IMAGE_POLICY_END: &str = "# END SUPERZEJ MANAGED IMAGE PREVIEW POLICY";
const IMAGE_POLICY_BLOCK: &str = r#"# BEGIN SUPERZEJ MANAGED IMAGE PREVIEW POLICY
# Keep image preview helpers such as ueberzugpp out of the default drawer. Text
# previews still work; set [drawer].image_previews = true to remove this block
# from the private generated config.
[plugin]
prepend_previewers = [
  { mime = "image/*", run = "noop" },
]
prepend_preloaders = [
  { mime = "image/*", run = "noop" },
]
# END SUPERZEJ MANAGED IMAGE PREVIEW POLICY"#;
/// `theme.toml` with an `{{ACCENT}}` placeholder (an `#rrggbb`), filled per-open.
const THEME_TMPL: &str = include_str!("../../../config/yazi/theme.toml");

/// The file manager the drawer runs: an explicit `[drawer] command`, else the
/// pinned yazi (`SUPERZEJ_YAZI_BIN`), else `yazi` on PATH.
pub fn bin(cfg: &Config) -> String {
    resolve_bin(&cfg.drawer.command, std::env::var("SUPERZEJ_YAZI_BIN").ok())
}

/// Pure binary resolution: a non-blank configured command wins, else the pinned
/// `SUPERZEJ_YAZI_BIN`, else `yazi` on PATH.
fn resolve_bin(configured: &str, env_bin: Option<String>) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    env_bin.unwrap_or_else(|| "yazi".into())
}

/// The drawer yazi's private config dir (`YAZI_CONFIG_HOME`), or `None` to use
/// the user's own system config. Empty config ⇒ `<superzej-dir>/yazi` (honors
/// `SUPERZEJ_DIR`, so dev/test instances stay isolated); "system"/"none" ⇒
/// `None`; anything else is used verbatim (tilde-expanded).
pub fn config_home(cfg: &Config) -> Option<PathBuf> {
    let v = cfg.drawer.config_home.trim();
    if v.eq_ignore_ascii_case("system") || v.eq_ignore_ascii_case("none") {
        return None;
    }
    if v.is_empty() {
        return Some(util::superzej_dir().join("yazi"));
    }
    Some(PathBuf::from(util::expand_tilde(v)))
}

/// Seed the bundled config into `dir` (once, never overwriting `yazi.toml` /
/// `keymap.toml` so user edits survive) and always (re)write the accent-derived
/// `theme.toml`. Best-effort: a failure just means yazi falls back to its
/// built-in defaults.
pub fn ensure_config(cfg: &Config) -> Option<PathBuf> {
    let dir = config_home(cfg)?;
    let _ = std::fs::create_dir_all(&dir);
    seed_once(&dir, "yazi.toml", YAZI_TOML);
    apply_image_preview_policy(&dir, cfg.drawer.image_previews);
    seed_once(&dir, "keymap.toml", KEYMAP_TOML);
    write_theme(&dir, &cfg.accent_hex());
    Some(dir)
}

/// Write `name` into `dir` only if it does not already exist.
fn seed_once(dir: &std::path::Path, name: &str, contents: &str) {
    let path = dir.join(name);
    if !path.exists() {
        let _ = std::fs::write(path, contents);
    }
}

/// Add or remove superzej's managed image-preview policy in `yazi.toml`.
/// Existing users may already have a private config seeded before the safe block
/// existed; append the block only when there is no `[plugin]` table to collide
/// with. Containment still protects user-customized configs we cannot rewrite.
fn apply_image_preview_policy(dir: &std::path::Path, enabled: bool) {
    let path = dir.join("yazi.toml");
    let Ok(body) = std::fs::read_to_string(&path) else {
        return;
    };
    let next = if enabled {
        remove_managed_image_policy(&body)
    } else if body.contains(IMAGE_POLICY_BEGIN) || body.lines().any(|l| l.trim() == "[plugin]") {
        body.clone()
    } else {
        format!("{}\n\n{}\n", body.trim_end(), IMAGE_POLICY_BLOCK)
    };
    if next != body {
        let _ = std::fs::write(path, next);
    }
}

fn remove_managed_image_policy(body: &str) -> String {
    let Some(start) = body.find(IMAGE_POLICY_BEGIN) else {
        return body.to_string();
    };
    let Some(end_rel) = body[start..].find(IMAGE_POLICY_END) else {
        return body.to_string();
    };
    let end = start + end_rel + IMAGE_POLICY_END.len();
    let mut next = String::with_capacity(body.len());
    next.push_str(body[..start].trim_end());
    next.push_str(body[end..].trim_start_matches(['\r', '\n']));
    if !next.ends_with('\n') {
        next.push('\n');
    }
    next
}

/// Regenerate `theme.toml` from the accent (an `#rrggbb`). Always overwritten —
/// it is derived state, not user config.
pub fn write_theme(dir: &std::path::Path, accent_hex: &str) {
    let theme = THEME_TMPL.replace("{{ACCENT}}", accent_hex);
    let _ = std::fs::write(dir.join("theme.toml"), theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn cfg_with(command: &str, config_home: &str) -> Config {
        let mut c = Config::default();
        c.drawer.command = command.into();
        c.drawer.config_home = config_home.into();
        c
    }

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sz-yazi-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolve_bin_precedence() {
        // Configured command wins.
        assert_eq!(resolve_bin("ranger", Some("/x/yazi".into())), "ranger");
        assert_eq!(resolve_bin("  nnn  ", None), "nnn"); // trimmed
        // Else the pinned binary.
        assert_eq!(
            resolve_bin("", Some("/nix/store/x/bin/yazi".into())),
            "/nix/store/x/bin/yazi"
        );
        assert_eq!(resolve_bin("   ", Some("/x/yazi".into())), "/x/yazi");
        // Else bare `yazi`.
        assert_eq!(resolve_bin("", None), "yazi");
    }

    #[test]
    fn bin_resolves_from_config() {
        // The public wrapper threads `[drawer] command` through resolve_bin.
        assert_eq!(bin(&cfg_with("ranger", "")), "ranger");
    }

    #[test]
    fn config_home_default_is_private_under_superzej_dir() {
        let home = config_home(&cfg_with("", "")).unwrap();
        assert_eq!(home, util::superzej_dir().join("yazi"));
    }

    #[test]
    fn config_home_system_opts_out() {
        assert!(config_home(&cfg_with("", "system")).is_none());
        assert!(config_home(&cfg_with("", "none")).is_none());
        assert!(config_home(&cfg_with("", "SYSTEM")).is_none()); // case-insensitive
    }

    #[test]
    fn config_home_explicit_is_tilde_expanded() {
        let h = config_home(&cfg_with("", "/tmp/custom-yazi")).unwrap();
        assert_eq!(h, PathBuf::from("/tmp/custom-yazi"));
        let tilde = config_home(&cfg_with("", "~/cfg")).unwrap();
        assert_eq!(tilde, PathBuf::from(util::expand_tilde("~/cfg")));
    }

    #[test]
    fn ensure_config_seeds_and_themes() {
        let dir = tmpdir();
        let mut cfg = cfg_with("", dir.to_str().unwrap());
        cfg.theme.accent = "#abcdef".into();

        let got = ensure_config(&cfg).unwrap();
        assert_eq!(got, dir);
        for f in ["yazi.toml", "keymap.toml", "theme.toml"] {
            assert!(dir.join(f).exists(), "{f} seeded");
        }
        // theme is rendered from the accent (placeholder substituted).
        let theme = std::fs::read_to_string(dir.join("theme.toml")).unwrap();
        assert!(theme.contains("#abcdef"));
        assert!(!theme.contains("{{ACCENT}}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_disables_image_previews_by_default() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());

        ensure_config(&cfg).unwrap();
        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert!(yazi.contains(IMAGE_POLICY_BEGIN));
        assert!(yazi.contains("mime = \"image/*\", run = \"noop\""));
        assert!(yazi.contains("prepend_preloaders"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_image_preview_opt_in_removes_managed_block() {
        let dir = tmpdir();
        let mut cfg = cfg_with("", dir.to_str().unwrap());
        ensure_config(&cfg).unwrap();

        cfg.drawer.image_previews = true;
        ensure_config(&cfg).unwrap();
        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert!(!yazi.contains(IMAGE_POLICY_BEGIN));
        assert!(!yazi.contains(IMAGE_POLICY_END));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_adds_policy_to_old_default_without_plugin_table() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        std::fs::write(
            dir.join("yazi.toml"),
            "[mgr]\nratio = [1, 3, 4]\n\n[preview]\ntab_size = 2\n",
        )
        .unwrap();

        ensure_config(&cfg).unwrap();
        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert!(yazi.contains(IMAGE_POLICY_BEGIN));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_does_not_append_policy_to_user_plugin_table() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        std::fs::write(dir.join("yazi.toml"), "[plugin]\nprepend_previewers = []\n").unwrap();

        ensure_config(&cfg).unwrap();
        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert!(!yazi.contains(IMAGE_POLICY_BEGIN));
        assert_eq!(yazi.matches("[plugin]").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn ensure_config_preserves_user_edits_while_applying_policy_and_regenerates_theme() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        ensure_config(&cfg).unwrap();

        // User edits yazi.toml/keymap.toml; stale theme placeholder left behind.
        std::fs::write(dir.join("yazi.toml"), "# my edits\n").unwrap();
        std::fs::write(dir.join("keymap.toml"), "# my keys\n").unwrap();
        std::fs::write(dir.join("theme.toml"), "stale {{ACCENT}}\n").unwrap();

        ensure_config(&cfg).unwrap();
        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert!(
            yazi.starts_with("# my edits\n"),
            "yazi.toml edits preserved"
        );
        assert!(yazi.contains(IMAGE_POLICY_BEGIN), "safe policy appended");
        assert_eq!(
            std::fs::read_to_string(dir.join("keymap.toml")).unwrap(),
            "# my keys\n",
            "keymap.toml preserved"
        );
        let theme = std::fs::read_to_string(dir.join("theme.toml")).unwrap();
        assert!(!theme.contains("{{ACCENT}}"), "theme.toml regenerated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_system_writes_nothing() {
        let cfg = cfg_with("", "system");
        assert!(ensure_config(&cfg).is_none());
    }

    #[test]
    fn write_theme_substitutes_every_accent() {
        let dir = tmpdir();
        write_theme(&dir, "#123456");
        let theme = std::fs::read_to_string(dir.join("theme.toml")).unwrap();
        assert!(!theme.contains("{{ACCENT}}"));
        assert!(theme.matches("#123456").count() >= 2); // appears on multiple surfaces
        let _ = std::fs::remove_dir_all(&dir);
    }
}
