//! The bottom file-manager drawer's yazi: a private, version-pinned build with a
//! private config dir, fully isolated from the user's system yazi.
//!
//! - binary: `THEGN_YAZI_BIN` (wired by Nix to the pinned `thegn-yazi`);
//! - config: `YAZI_CONFIG_HOME` = `<thegn-dir>/yazi` by default, seeded once
//!   from the bundled defaults below and never overwritten. Only the derived
//!   `theme.toml` is regenerated, from the thegn accent, so the drawer matches
//!   the palette.

use crate::config::Config;
use crate::util;
use std::path::PathBuf;

/// Bundled yazi config, embedded in the binary and seeded at first launch.
const YAZI_TOML: &str = include_str!("../../../config/yazi/yazi.toml");
const KEYMAP_TOML: &str = include_str!("../../../config/yazi/keymap.toml");
/// The managed yazi.toml block that disables image preview/preload helpers.
const IMAGE_POLICY_BEGIN: &str = "# BEGIN THEGN MANAGED IMAGE PREVIEW POLICY";
const IMAGE_POLICY_END: &str = "# END THEGN MANAGED IMAGE PREVIEW POLICY";
const IMAGE_POLICY_BLOCK: &str = r#"# BEGIN THEGN MANAGED IMAGE PREVIEW POLICY
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
# END THEGN MANAGED IMAGE PREVIEW POLICY"#;
/// `theme.toml` with an `{{ACCENT}}` placeholder (an `#rrggbb`), filled per-open.
const THEME_TMPL: &str = include_str!("../../../config/yazi/theme.toml");

/// The vendored `git.yazi` plugin (MIT, yazi-rs), seeded so the drawer can show
/// git status as a linemode (item 606).
const GIT_PLUGIN_LUA: &str = include_str!("../../../config/yazi/plugins/git.yazi/main.lua");

/// The drawer-control plugins: tiny emitters that write thegn's private
/// `OSC 5379` command on yazi's own PTY so the host can drive the chrome while
/// yazi keeps ownership of every key (see `keymap.toml` + host `drawer_command`).
/// Derived state — always refreshed like `git.yazi`, never user config.
const DRAWER_PLUGINS: &[(&str, &str)] = &[
    (
        "sz-drawer-close.yazi",
        include_str!("../../../config/yazi/plugins/sz-drawer-close.yazi/main.lua"),
    ),
    (
        "sz-drawer-editor.yazi",
        include_str!("../../../config/yazi/plugins/sz-drawer-editor.yazi/main.lua"),
    ),
];
const GIT_POLICY_BEGIN: &str = "# BEGIN THEGN MANAGED GIT STATUS POLICY";
const GIT_POLICY_END: &str = "# END THEGN MANAGED GIT STATUS POLICY";
/// `prepend_fetchers` registering the git plugin. Array-of-tables syntax so it
/// extends the existing `[plugin]` table (the image policy block) without a
/// duplicate-table TOML error. No `id` field — the pinned yazi is > v26.1.22.
const GIT_FETCHERS_BLOCK: &str = r#"# BEGIN THEGN MANAGED GIT STATUS POLICY
# Show git status as a drawer linemode (item 606): the vendored git.yazi plugin
# under plugins/ is registered as a fetcher here and initialised in init.lua.
# Set [drawer].git_status = false to remove this block from the generated config.
[[plugin.prepend_fetchers]]
url = "*"
run = "git"

[[plugin.prepend_fetchers]]
url = "*/"
run = "git"
# END THEGN MANAGED GIT STATUS POLICY"#;
const GIT_INIT_BEGIN: &str = "-- BEGIN THEGN MANAGED GIT STATUS INIT";
const GIT_INIT_END: &str = "-- END THEGN MANAGED GIT STATUS INIT";
const GIT_INIT_BLOCK: &str = r#"-- BEGIN THEGN MANAGED GIT STATUS INIT
require("git"):setup { order = 1500 }
-- END THEGN MANAGED GIT STATUS INIT"#;

/// The file manager the drawer runs: an explicit `[drawer] command`, else the
/// pinned yazi (`THEGN_YAZI_BIN`), else `yazi` on PATH.
pub fn bin(cfg: &Config) -> String {
    resolve_bin(&cfg.drawer.command, std::env::var("THEGN_YAZI_BIN").ok())
}

/// Pure binary resolution: a non-blank configured command wins, else the pinned
/// `THEGN_YAZI_BIN`, else `yazi` on PATH.
fn resolve_bin(configured: &str, env_bin: Option<String>) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    env_bin.unwrap_or_else(|| "yazi".into())
}

/// The drawer yazi's private config dir (`YAZI_CONFIG_HOME`), or `None` to use
/// the user's own system config. Empty config ⇒ `<thegn-dir>/yazi` (honors
/// `THEGN_DIR`, so dev/test instances stay isolated); "system"/"none" ⇒
/// `None`; anything else is used verbatim (tilde-expanded).
pub fn config_home(cfg: &Config) -> Option<PathBuf> {
    let v = cfg.drawer.config_home.trim();
    if v.eq_ignore_ascii_case("system") || v.eq_ignore_ascii_case("none") {
        return None;
    }
    if v.is_empty() {
        return Some(util::thegn_dir().join("yazi"));
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
    apply_git_status_policy(&dir, cfg.drawer.git_status);
    apply_drawer_control(&dir);
    seed_once(&dir, "keymap.toml", KEYMAP_TOML);
    write_theme(&dir, &cfg.accent_hex());
    Some(dir)
}

/// (Re)write the drawer-control plugins (derived state) and migrate a stale
/// keymap: earlier builds seeded `keymap.toml` bindings that shelled out to
/// removed `thegn files`/`thegn tool` subcommands. `keymap.toml` is
/// seed-once (user-editable), so drop it when it still carries those dead
/// commands — the caller's `seed_once` then rewrites the current, plugin-based
/// bindings. A user's own keymap (without those strings) is left untouched.
fn apply_drawer_control(dir: &std::path::Path) {
    let pdir = dir.join("plugins");
    for (name, lua) in DRAWER_PLUGINS {
        let p = pdir.join(name);
        let _ = std::fs::create_dir_all(&p);
        let _ = std::fs::write(p.join("main.lua"), lua);
    }
    let keymap = dir.join("keymap.toml");
    if let Ok(body) = std::fs::read_to_string(&keymap)
        && (body.contains("thegn files") || body.contains("thegn tool"))
    {
        let _ = std::fs::remove_file(&keymap);
    }
}

/// Add or remove thegn's managed git-status integration (item 606). When
/// enabled: (re)write the vendored `git.yazi` plugin (derived state), register
/// its fetchers in `yazi.toml`, and initialise it in `init.lua`. When disabled:
/// strip the managed blocks (the plugin files are left in place, inert without
/// the fetcher). Best-effort — failures just leave git status off.
fn apply_git_status_policy(dir: &std::path::Path, enabled: bool) {
    let toml = dir.join("yazi.toml");
    let init = dir.join("init.lua");
    if enabled {
        let pdir = dir.join("plugins").join("git.yazi");
        let _ = std::fs::create_dir_all(&pdir);
        // The plugin is vendored, not user config: always refresh it so a pinned
        // yazi/plugin bump lands (mirrors theme.toml's regenerate-always policy).
        let _ = std::fs::write(pdir.join("main.lua"), GIT_PLUGIN_LUA);
        ensure_managed_block(&toml, GIT_POLICY_BEGIN, GIT_FETCHERS_BLOCK);
        ensure_managed_block(&init, GIT_INIT_BEGIN, GIT_INIT_BLOCK);
    } else {
        remove_managed_block(&toml, GIT_POLICY_BEGIN, GIT_POLICY_END);
        remove_managed_block(&init, GIT_INIT_BEGIN, GIT_INIT_END);
    }
}

/// Append a `begin..end`-delimited managed block to `path` (creating the file if
/// absent) when it isn't already present. Idempotent: a present block is left
/// untouched so user edits around it survive.
fn ensure_managed_block(path: &std::path::Path, begin: &str, block: &str) {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    if body.contains(begin) {
        return;
    }
    let next = if body.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{}\n", body.trim_end(), block)
    };
    let _ = std::fs::write(path, next);
}

/// Remove a `begin..end`-delimited managed block from `path` (no-op if absent).
fn remove_managed_block(path: &std::path::Path, begin: &str, end: &str) {
    let Ok(body) = std::fs::read_to_string(path) else {
        return;
    };
    let (Some(start), Some(end_at)) = (body.find(begin), body.find(end)) else {
        return;
    };
    let end = end_at + end.len();
    let mut next = String::with_capacity(body.len());
    next.push_str(body[..start].trim_end());
    next.push_str(body[end..].trim_start_matches(['\r', '\n']));
    let next = next.trim_start_matches('\n').to_string();
    let next = if next.is_empty() {
        String::new()
    } else if next.ends_with('\n') {
        next
    } else {
        format!("{next}\n")
    };
    let _ = std::fs::write(path, next);
}

/// Write `name` into `dir` only if it does not already exist.
fn seed_once(dir: &std::path::Path, name: &str, contents: &str) {
    let path = dir.join(name);
    if !path.exists() {
        let _ = std::fs::write(path, contents);
    }
}

/// Add or remove thegn's managed image-preview policy in `yazi.toml`.
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
    fn config_home_default_is_private_under_thegn_dir() {
        let home = config_home(&cfg_with("", "")).unwrap();
        assert_eq!(home, util::thegn_dir().join("yazi"));
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
    fn ensure_config_seeds_drawer_control_plugins_and_keymap() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        ensure_config(&cfg).unwrap();

        // Both control plugins are vendored under plugins/.
        for name in ["sz-drawer-close.yazi", "sz-drawer-editor.yazi"] {
            let lua = dir.join("plugins").join(name).join("main.lua");
            assert!(lua.exists(), "{name} seeded");
            assert!(std::fs::read_to_string(&lua).unwrap().contains("5379"));
        }
        // The seeded keymap drives the plugins, not any removed subcommand.
        let keymap = std::fs::read_to_string(dir.join("keymap.toml")).unwrap();
        assert!(keymap.contains("plugin sz-drawer-close"));
        assert!(keymap.contains("plugin sz-drawer-editor"));
        assert!(!keymap.contains("thegn files"));
        assert!(!keymap.contains("thegn tool"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_migrates_stale_keymap_with_dead_commands() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        // Simulate a config seeded by an older build (dead `thegn …` shell).
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("keymap.toml"),
            "[mgr]\nprepend_keymap = [\n  { on = \"q\", run = 'shell \"thegn files --close\" --orphan' },\n]\n",
        )
        .unwrap();

        ensure_config(&cfg).unwrap();
        let keymap = std::fs::read_to_string(dir.join("keymap.toml")).unwrap();
        assert!(!keymap.contains("thegn files"), "dead binding migrated out");
        assert!(
            keymap.contains("plugin sz-drawer-close"),
            "fresh binding seeded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_config_keeps_a_custom_user_keymap() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        // A user keymap without our dead strings must survive untouched.
        std::fs::write(dir.join("keymap.toml"), "# my keys\n").unwrap();

        ensure_config(&cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("keymap.toml")).unwrap(),
            "# my keys\n",
        );
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
    fn ensure_config_seeds_git_status_by_default() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        ensure_config(&cfg).unwrap();

        // Plugin vendored under plugins/git.yazi/main.lua.
        let plugin = dir.join("plugins").join("git.yazi").join("main.lua");
        assert!(plugin.exists(), "git.yazi plugin seeded");
        let lua = std::fs::read_to_string(&plugin).unwrap();
        assert!(lua.contains("return { setup = setup, fetch = fetch }"));

        // Fetchers registered in yazi.toml; setup wired in init.lua.
        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert!(yazi.contains(GIT_POLICY_BEGIN));
        assert!(yazi.contains("[[plugin.prepend_fetchers]]"));
        assert!(yazi.contains("run = \"git\""));
        let init = std::fs::read_to_string(dir.join("init.lua")).unwrap();
        assert!(init.contains("require(\"git\"):setup"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_status_block_is_idempotent_and_keeps_one_plugin_table() {
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        ensure_config(&cfg).unwrap();
        ensure_config(&cfg).unwrap(); // second pass must not duplicate

        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert_eq!(
            yazi.matches(GIT_POLICY_BEGIN).count(),
            1,
            "no duplicate block"
        );
        // Image policy owns the only literal `[plugin]` table; git uses
        // array-of-tables, so the config stays valid TOML.
        assert_eq!(yazi.matches("\n[plugin]\n").count(), 1);
        let init = std::fs::read_to_string(dir.join("init.lua")).unwrap();
        assert_eq!(init.matches(GIT_INIT_BEGIN).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_yazi_toml_stays_valid_with_image_and_git_blocks() {
        // The git fetchers use array-of-tables so they extend the image policy's
        // `[plugin]` table instead of duplicating it. A regression here would
        // make yazi silently fall back to presets, so assert the file parses.
        let dir = tmpdir();
        let cfg = cfg_with("", dir.to_str().unwrap());
        ensure_config(&cfg).unwrap();
        let body = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&body).expect("generated yazi.toml is valid TOML");
        let fetchers = parsed["plugin"]["prepend_fetchers"]
            .as_array()
            .expect("prepend_fetchers array");
        assert_eq!(fetchers.len(), 2, "two git fetchers registered");
        // Image policy keys remain on the same table.
        assert!(parsed["plugin"].get("prepend_previewers").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_status_opt_out_removes_managed_blocks() {
        let dir = tmpdir();
        let mut cfg = cfg_with("", dir.to_str().unwrap());
        ensure_config(&cfg).unwrap();

        cfg.drawer.git_status = false;
        ensure_config(&cfg).unwrap();
        let yazi = std::fs::read_to_string(dir.join("yazi.toml")).unwrap();
        assert!(!yazi.contains(GIT_POLICY_BEGIN));
        assert!(!yazi.contains(GIT_POLICY_END));
        assert!(!yazi.contains("[[plugin.prepend_fetchers]]"));
        // The image policy block survives the git-block removal.
        assert!(yazi.contains(IMAGE_POLICY_BEGIN));
        let init = std::fs::read_to_string(dir.join("init.lua")).unwrap();
        assert!(!init.contains(GIT_INIT_BEGIN));
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
