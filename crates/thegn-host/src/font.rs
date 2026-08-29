use crate::palette::PaletteItem;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const RECOMMENDED_FONTS: &[&str] = &[
    "VictorMono Nerd Font",
    "JetBrainsMono Nerd Font",
    "CaskaydiaCove Nerd Font",
    "SauceCodePro Nerd Font",
    "Monoid Nerd Font",
    "Iosevka Nerd Font",
    "Inconsolata Nerd Font",
    "Hack Nerd Font",
    "FiraCode Nerd Font",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontRow {
    pub family: String,
    pub label: String,
}

pub fn font_palette_items() -> Result<Vec<PaletteItem>, String> {
    let rows = match fc_list_rows() {
        Ok(rows) => rows,
        // Stock macOS has no fontconfig, so `fc-list` is simply absent and the
        // picker used to dead-end on "fc-list failed: No such file". Fall back to
        // the standard font directories there; elsewhere, surface the real error.
        Err(e) if cfg!(target_os = "macos") => {
            let rows = font_rows_from_dirs(&macos_font_dirs());
            if rows.is_empty() {
                return Err(format!("{e}; no fonts found under ~/Library/Fonts"));
            }
            rows
        }
        Err(e) => return Err(e),
    };
    Ok(rows
        .into_iter()
        .map(|row| PaletteItem::new(format!("font:{}", row.family), row.label))
        .collect())
}

/// Enumerate families via fontconfig. `Err` when `fc-list` is missing or fails.
fn fc_list_rows() -> Result<Vec<FontRow>, String> {
    // Accepted on-loop subprocess: `fc-list` is ms-scale and only runs on the
    // explicit SwitchFont action. Revisit if font enumeration ever grows.
    #[expect(clippy::disallowed_methods)]
    let output = std::process::Command::new("fc-list")
        .args([":", "family"])
        .output()
        .map_err(|e| format!("fc-list failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "fc-list exited with {}",
            output.status.code().unwrap_or_default()
        ));
    }
    Ok(font_rows_from_fc_list(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// The three directories macOS resolves fonts from, user-first.
fn macos_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Fonts"));
    }
    dirs.push(PathBuf::from("/Library/Fonts"));
    dirs.push(PathBuf::from("/System/Library/Fonts"));
    dirs
}

/// How deep to descend under each font directory.
///
/// macOS resolves its font directories **recursively**, and a flat `read_dir`
/// misses most of what is actually installed. Measured on macOS 26:
///   * depth 1 — `/System/Library/Fonts/Supplemental/`, where the bulk of the
///     shipped system faces live: 81 → 370 files;
///   * depth 6 — nix-darwin's `/Library/Fonts/Nix Fonts/<hash>-<pkg>/share/
///     fonts/opentype/`;
///   * depth 8 — the Nerd Font packages' extra `truetype/NerdFonts/<Family>/`
///     nesting. This is where FiraCode Nerd Font actually sits, so a flat scan
///     reported **zero** of `RECOMMENDED_FONTS` as available on a machine that
///     had one installed.
///
/// 8 is a real bound, not "deep enough for now": the deepest font on that
/// machine is at 8, and the whole walk costs ~0.5 ms (vs 0.15 ms flat) on the
/// explicit `SwitchFont` action only. The cap is what keeps a font picker from
/// becoming a filesystem walk if someone points a font dir at their home.
const FONT_SCAN_DEPTH: usize = 8;

/// Derive families from font FILENAMES under `dirs` — the fontconfig-free
/// fallback. Descends [`FONT_SCAN_DEPTH`] levels; see there for why flat was wrong.
///
/// Reading real family names would mean parsing each font's `name` table; the
/// filename is a good enough key here because the only consumer writes the
/// chosen string into an alacritty `font.normal.family`, and Nerd Font
/// distributions name their files after the family they register. A style suffix
/// (`-Regular`, ` Bold Italic`) is stripped so all faces of a family collapse to
/// one entry, matching what `fc-list : family` yields.
fn font_rows_from_dirs(dirs: &[PathBuf]) -> Vec<FontRow> {
    let mut families = BTreeSet::new();
    for dir in dirs {
        collect_font_families(dir, FONT_SCAN_DEPTH, &mut families);
    }
    rank_families(families)
}

/// Add every font family found in `dir` to `families`, descending at most
/// `depth` more levels. An unreadable directory is skipped — `/Library/Fonts`
/// may not exist, and a font dir we can't read is not an error worth surfacing
/// in a picker.
fn collect_font_families(dir: &Path, depth: usize, families: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        // `file_type` avoids a stat per entry on the common (file) path and,
        // unlike `is_dir`, does not follow symlinks — a font dir that links to
        // itself must not send this into a loop.
        if ent.file_type().is_ok_and(|t| t.is_dir()) {
            if depth > 0 {
                collect_font_families(&ent.path(), depth - 1, families);
            }
            continue;
        }
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if !matches!(
            ext.to_ascii_lowercase().as_str(),
            "ttf" | "otf" | "ttc" | "dfont"
        ) {
            continue;
        }
        let family = family_from_font_filename(stem);
        if !family.is_empty() && !is_short_nerd_font_alias(&family) {
            families.insert(family);
        }
    }
}

/// Strip a style suffix from a font filename stem, yielding the family.
/// `"JetBrainsMonoNerdFont-BoldItalic"` → `"JetBrainsMonoNerdFont"`.
fn family_from_font_filename(stem: &str) -> String {
    const STYLES: &[&str] = &[
        "thin",
        "extralight",
        "ultralight",
        "light",
        "regular",
        "book",
        "medium",
        "semibold",
        "demibold",
        "bold",
        "extrabold",
        "black",
        "heavy",
        "italic",
        "oblique",
    ];
    // Split on the last '-' (the near-universal `Family-Style` convention) and
    // drop the tail only when every word in it is a style token, so a family
    // that legitimately contains a hyphen survives.
    let base = match stem.rsplit_once('-') {
        Some((head, tail)) if !head.is_empty() && is_all_styles(tail, STYLES) => head,
        _ => stem,
    };
    base.trim().to_string()
}

/// Whether `s` is made up entirely of style words (camelCase or space/underscore
/// separated), e.g. `"BoldItalic"`, `"Semi Bold"`, `"regular"`.
fn is_all_styles(s: &str, styles: &[&str]) -> bool {
    let lower = s.to_ascii_lowercase();
    let mut rest = lower.replace([' ', '_'], "");
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        // Longest match first, so "extrabold" isn't consumed as "bold".
        let Some(hit) = styles
            .iter()
            .filter(|st| rest.starts_with(**st))
            .max_by_key(|st| st.len())
        else {
            return false;
        };
        rest = rest[hit.len()..].to_string();
    }
    true
}

pub fn font_rows_from_fc_list(fc_list: &str) -> Vec<FontRow> {
    let mut families = BTreeSet::new();
    for line in fc_list.lines() {
        let rest = line.split_once(':').map(|(_, rest)| rest).unwrap_or(line);
        let family_segment = rest
            .split_once(":style=")
            .map(|(families, _)| families)
            .unwrap_or(rest);
        for family in family_segment.split(',').map(str::trim) {
            if family.is_empty() || is_short_nerd_font_alias(family) {
                continue;
            }
            families.insert(family.to_string());
        }
    }
    rank_families(families)
}

/// Label + order a deduped family set: recommended fonts first (in
/// `RECOMMENDED_FONTS` order), then everything else case-insensitively. Shared by
/// both enumeration paths so the picker looks identical either way.
fn rank_families(families: BTreeSet<String>) -> Vec<FontRow> {
    let recommended_order: BTreeMap<String, usize> = RECOMMENDED_FONTS
        .iter()
        .enumerate()
        .map(|(idx, name)| (normalize_family(name), idx))
        .collect();

    let mut rows: Vec<_> = families
        .into_iter()
        .map(|family| {
            let recommended_idx = recommended_order.get(&normalize_family(&family)).copied();
            let label = if recommended_idx.is_some() {
                format!("★ Recommended — {family}")
            } else {
                family.clone()
            };
            (
                recommended_idx,
                family.to_ascii_lowercase(),
                FontRow { family, label },
            )
        })
        .collect();
    rows.sort_by(|a, b| match (a.0, b.0) {
        (Some(ai), Some(bi)) => ai.cmp(&bi).then_with(|| a.1.cmp(&b.1)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    rows.into_iter().map(|(_, _, row)| row).collect()
}

/// The Alacritty config the font picker writes to: `$THEGN_ALACRITTY_CONFIG`
/// (set by the `.app` launcher, but only when Alacritty is the terminal that
/// actually runs), else Alacritty's own XDG location.
///
/// `None` when neither exists — the caller must decline rather than guess. The
/// previous default was the **relative** path `config/alacritty.toml`, resolved
/// against the process CWD: in a compositor whose CWD is whichever worktree tab
/// is focused, that either failed to open or silently edited a file in some
/// unrelated checkout (the thegn repo itself, most often).
pub fn alacritty_config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("THEGN_ALACRITTY_CONFIG") {
        return Some(PathBuf::from(p));
    }
    // Alacritty reads `$XDG_CONFIG_HOME/alacritty/alacritty.toml` on every
    // platform (and also `~/.config/...` on macOS, which is where thegn's own
    // `xdg_config_home` points by default).
    let candidate = thegn_core::util::xdg_config_home().join("alacritty/alacritty.toml");
    candidate.is_file().then_some(candidate)
}

/// The terminal the font picker is being asked to reconfigure.
///
/// Resolved from the terminal that is **actually running**, not the one the
/// installer happened to pick: `TERM_PROGRAM`, falling back to `LC_TERMINAL`
/// (which survives ssh) and then `TERM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    Alacritty,
    Ghostty,
    Kitty,
    /// Identified, but its font cannot be set by editing a text config:
    /// WezTerm's is Lua (a parse-and-rewrite problem with no safe fallback),
    /// and Terminal.app / iTerm2 keep theirs in a plist or dynamic profile that
    /// needs `defaults write` plus a restart. Declining with instructions beats
    /// half-editing someone's config.
    Unsupported(&'static str),
    Unknown,
}

/// Identify the running terminal from its own environment.
pub fn detect_terminal(term_program: Option<&str>, term: Option<&str>) -> TerminalKind {
    let hay = format!(
        "{} {}",
        term_program.unwrap_or("").to_ascii_lowercase(),
        term.unwrap_or("").to_ascii_lowercase()
    );
    // Order matters only in that each name is distinctive; no substring of one
    // terminal's name appears in another's.
    if hay.contains("alacritty") {
        TerminalKind::Alacritty
    } else if hay.contains("ghostty") {
        TerminalKind::Ghostty
    } else if hay.contains("kitty") {
        TerminalKind::Kitty
    } else if hay.contains("wezterm") {
        TerminalKind::Unsupported("WezTerm")
    } else if hay.contains("iterm") {
        TerminalKind::Unsupported("iTerm2")
    } else if hay.contains("apple_terminal") {
        TerminalKind::Unsupported("Terminal.app")
    } else {
        TerminalKind::Unknown
    }
}

/// How to tell a user to set the font themselves, for a terminal thegn will not
/// edit. Actionable rather than apologetic — the exact line, in the exact file.
fn manual_instructions(name: &str, family: &str) -> String {
    match name {
        "WezTerm" => format!(
            "thegn can't set WezTerm's font (its config is Lua). Add to ~/.wezterm.lua:              config.font = wezterm.font('{family}')"
        ),
        "iTerm2" => {
            format!("thegn can't set iTerm2's font. Settings → Profiles → Text → Font → {family}")
        }
        _ => format!(
            "thegn can't set Terminal.app's font. Settings → Profiles → Text → Font → {family}"
        ),
    }
}

/// Ghostty's config file. macOS keeps it under Application Support; every other
/// platform uses XDG.
fn ghostty_config_path() -> PathBuf {
    if cfg!(target_os = "macos")
        && let Some(home) = std::env::var_os("HOME")
    {
        let p =
            PathBuf::from(&home).join("Library/Application Support/com.mitchellh.ghostty/config");
        if p.is_file() {
            return p;
        }
    }
    thegn_core::util::xdg_config_home().join("ghostty/config")
}

fn kitty_config_path() -> PathBuf {
    thegn_core::util::xdg_config_home().join("kitty/kitty.conf")
}

/// Set the terminal's font family, for whichever terminal is actually running.
///
/// Previously this always patched an Alacritty config — 4th of the 5 terminals
/// the macOS `.app` launcher will start, and not the one it prefers — so on a
/// Ghostty session it edited a file nothing was reading and reported success.
pub fn apply_font_family(family: &str) -> Result<PathBuf, String> {
    let env = thegn_core::termcaps::TermEnv::from_env();
    apply_font_family_for(
        detect_terminal(env.program_name(), env.term.as_deref()),
        family,
    )
}

/// [`apply_font_family`] with the terminal chosen by the caller — the seam the
/// tests drive.
pub fn apply_font_family_for(kind: TerminalKind, family: &str) -> Result<PathBuf, String> {
    match kind {
        TerminalKind::Alacritty => {
            let path = alacritty_config_path().ok_or_else(|| {
                "no Alacritty config found — set THEGN_ALACRITTY_CONFIG, or create \
                 ~/.config/alacritty/alacritty.toml"
                    .to_string()
            })?;
            apply_font_family_to_path(&path, family)?;
            Ok(path)
        }
        TerminalKind::Ghostty => {
            let path = ghostty_config_path();
            write_simple_key(
                &path,
                "font-family",
                &format!("font-family = {family}"),
                family,
            )?;
            Ok(path)
        }
        TerminalKind::Kitty => {
            let path = kitty_config_path();
            write_simple_key(
                &path,
                "font_family",
                &format!("font_family {family}"),
                family,
            )?;
            Ok(path)
        }
        TerminalKind::Unsupported(name) => Err(manual_instructions(name, family)),
        TerminalKind::Unknown => Err(format!(
            "thegn doesn't know how to set the font for this terminal — set it to \
             {family} yourself"
        )),
    }
}

/// Patch a `key value` / `key = value` line-oriented config (Ghostty, kitty),
/// appending it when absent. Creates the file (and its directory) if needed —
/// both terminals treat a missing config as empty defaults, so writing one is
/// the same as editing it.
fn write_simple_key(path: &Path, key: &str, new_line: &str, family: &str) -> Result<(), String> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in current.lines() {
        let t = line.trim_start();
        // Only an active (uncommented) assignment of this key is replaced, so a
        // commented example in the user's config stays as documentation.
        let is_key = !t.starts_with('#')
            && t.starts_with(key)
            && t[key.len()..].starts_with(|c: char| c.is_whitespace() || c == '=');
        if is_key && !replaced {
            out.push(new_line.to_string());
            replaced = true;
        } else if !is_key {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(new_line.to_string());
    }
    let _ = family;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create {} failed: {e}", dir.display()))?;
    }
    let mut body = out.join("\n");
    body.push('\n');
    std::fs::write(path, body).map_err(|e| format!("write {} failed: {e}", path.display()))
}

fn apply_font_family_to_path(path: &Path, family: &str) -> Result<(), String> {
    let current = std::fs::read_to_string(path)
        .map_err(|e| format!("read {} failed: {e}", path.display()))?;
    let patched = patch_alacritty_font_family(&current, family)?;
    std::fs::write(path, patched).map_err(|e| format!("write {} failed: {e}", path.display()))
}

pub fn patch_alacritty_font_family(input: &str, family: &str) -> Result<String, String> {
    let escaped = family.replace('\\', "\\\\").replace('"', "\\\"");
    let mut changed = false;
    let mut out = Vec::new();
    for line in input.lines() {
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') && trimmed.starts_with("normal = { family = ") {
            out.push(format!("{indent}normal = {{ family = \"{escaped}\" }}"));
            changed = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !changed {
        return Err("no alacritty [font] normal.family line found".into());
    }
    let mut rendered = out.join("\n");
    if input.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

fn normalize_family(family: &str) -> String {
    family
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_short_nerd_font_alias(family: &str) -> bool {
    let lower = family.to_ascii_lowercase();
    lower.ends_with(" nf") || lower.ends_with(" nfm") || lower.ends_with(" nfp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_filenames_collapse_their_style_suffix_to_one_family() {
        // All faces of a family collapse to the same key…
        for stem in [
            "JetBrainsMonoNerdFont-Regular",
            "JetBrainsMonoNerdFont-Bold",
            "JetBrainsMonoNerdFont-BoldItalic",
            "JetBrainsMonoNerdFont-ExtraLight",
            "JetBrainsMonoNerdFont-Thin",
        ] {
            assert_eq!(
                family_from_font_filename(stem),
                "JetBrainsMonoNerdFont",
                "stem: {stem}"
            );
        }
        // …a bare family keeps its name…
        assert_eq!(family_from_font_filename("Menlo"), "Menlo");
        // …and a hyphen that is NOT a style suffix must survive, or families
        // like these would be silently truncated.
        assert_eq!(
            family_from_font_filename("Noto-Sans-Mono"),
            "Noto-Sans-Mono"
        );
        assert_eq!(family_from_font_filename("SF-Mono"), "SF-Mono");
    }

    #[test]
    fn style_suffix_detection_matches_longest_token_first() {
        const STYLES: &[&str] = &["bold", "extrabold", "italic", "regular", "semibold"];
        // "extrabold" must not be consumed as "extra" + "bold" (no "extra" token)
        // nor leave a dangling remainder.
        assert!(is_all_styles("ExtraBold", STYLES));
        assert!(is_all_styles("BoldItalic", STYLES));
        assert!(is_all_styles("Semi_Bold", STYLES) || is_all_styles("SemiBold", STYLES));
        assert!(!is_all_styles("Mono", STYLES));
        assert!(!is_all_styles("BoldMono", STYLES));
        assert!(!is_all_styles("", STYLES));
    }

    #[test]
    fn terminal_detection_prefers_the_running_terminal() {
        use TerminalKind::*;
        // TERM_PROGRAM is the first-hand answer…
        assert_eq!(
            detect_terminal(Some("ghostty"), Some("xterm-256color")),
            Ghostty
        );
        assert_eq!(
            detect_terminal(Some("Apple_Terminal"), None),
            Unsupported("Terminal.app")
        );
        assert_eq!(
            detect_terminal(Some("iTerm.app"), None),
            Unsupported("iTerm2")
        );
        assert_eq!(
            detect_terminal(Some("WezTerm"), None),
            Unsupported("WezTerm")
        );
        // …and TERM carries it when TERM_PROGRAM does not (kitty, alacritty).
        assert_eq!(detect_terminal(None, Some("xterm-kitty")), Kitty);
        assert_eq!(detect_terminal(None, Some("alacritty")), Alacritty);
        // Nothing recognisable ⇒ decline rather than guess a config to edit.
        assert_eq!(detect_terminal(None, Some("xterm-256color")), Unknown);
        assert_eq!(detect_terminal(None, None), Unknown);
    }

    #[test]
    fn unsupported_terminals_decline_with_instructions_and_write_nothing() {
        // WezTerm's config is Lua and Terminal.app/iTerm2 keep theirs in a
        // plist — half-editing someone's config is worse than declining, so
        // these must fail with the exact line to add themselves.
        for (kind, needle) in [
            (TerminalKind::Unsupported("WezTerm"), "wezterm.font"),
            (TerminalKind::Unsupported("iTerm2"), "Profiles"),
            (TerminalKind::Unsupported("Terminal.app"), "Profiles"),
        ] {
            let err = apply_font_family_for(kind, "Hack Nerd Font").unwrap_err();
            assert!(err.contains("Hack Nerd Font"), "{err}");
            assert!(err.contains(needle), "{err}");
        }
        let err = apply_font_family_for(TerminalKind::Unknown, "Hack Nerd Font").unwrap_err();
        assert!(err.contains("Hack Nerd Font"), "{err}");
    }

    #[test]
    fn simple_key_configs_are_patched_in_place_and_created_when_absent() {
        let tmp = tempfile::tempdir().unwrap();

        // Ghostty: replace the ACTIVE assignment, leave a commented example as
        // documentation, and preserve every unrelated line.
        let g = tmp.path().join("ghostty/config");
        std::fs::create_dir_all(g.parent().unwrap()).unwrap();
        std::fs::write(
            &g,
            "# font-family = Old Example\nfont-family = Menlo\ntheme = dark\n",
        )
        .unwrap();
        write_simple_key(
            &g,
            "font-family",
            "font-family = Hack Nerd Font",
            "Hack Nerd Font",
        )
        .unwrap();
        let out = std::fs::read_to_string(&g).unwrap();
        assert!(out.contains("font-family = Hack Nerd Font"), "{out}");
        assert!(!out.contains("font-family = Menlo"), "{out}");
        assert!(
            out.contains("# font-family = Old Example"),
            "comment kept: {out}"
        );
        assert!(out.contains("theme = dark"), "unrelated keys kept: {out}");
        // Idempotent: applying twice must not accumulate duplicate keys.
        write_simple_key(
            &g,
            "font-family",
            "font-family = Hack Nerd Font",
            "Hack Nerd Font",
        )
        .unwrap();
        let out2 = std::fs::read_to_string(&g).unwrap();
        assert_eq!(out2.matches("font-family = Hack").count(), 1, "{out2}");

        // kitty uses `key value` with no `=`, and a missing file is the same as
        // an empty one — both terminals treat absence as defaults.
        let k = tmp.path().join("kitty/kitty.conf");
        write_simple_key(
            &k,
            "font_family",
            "font_family Iosevka Nerd Font",
            "Iosevka Nerd Font",
        )
        .unwrap();
        let out = std::fs::read_to_string(&k).unwrap();
        assert_eq!(out.trim(), "font_family Iosevka Nerd Font");
    }

    #[test]
    fn dir_enumeration_finds_fonts_macos_actually_installs() {
        // The three real layouts a flat `read_dir` missed, at their real depths
        // (copied from an actual nix-darwin Mac — a shallower fixture would have
        // passed against a `FONT_SCAN_DEPTH` that still misses in the field,
        // which is exactly the mistake this fixture exists to prevent).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Menlo.ttc"), b"").unwrap();

        // depth 1 — /System/Library/Fonts/Supplemental
        let supp = root.join("Supplemental");
        std::fs::create_dir_all(&supp).unwrap();
        std::fs::write(supp.join("Courier New.ttf"), b"").unwrap();

        // depth 6 — Nix Fonts/<hash>-<pkg>/share/fonts/opentype
        let nix_otf = root.join("Nix Fonts/abc123-jetbrains-mono-2.304/share/fonts/opentype");
        std::fs::create_dir_all(&nix_otf).unwrap();
        std::fs::write(nix_otf.join("JetBrainsMono-Regular.otf"), b"").unwrap();

        // depth 8 — …/share/fonts/truetype/NerdFonts/<Family>, where the Nerd
        // Font packages actually put their files.
        let nix_nf = root.join(
            "Nix Fonts/def456-nerd-fonts-fira-code-3.5.0/share/fonts/truetype/NerdFonts/FiraCode",
        );
        std::fs::create_dir_all(&nix_nf).unwrap();
        std::fs::write(nix_nf.join("FiraCodeNerdFont-Regular.ttf"), b"").unwrap();
        std::fs::write(nix_nf.join("FiraCodeNerdFont-Bold.ttf"), b"").unwrap();

        // One level past the budget — proves the depth cap is real, so a font
        // directory can never turn the picker into a filesystem walk.
        let deep = nix_nf.join("too/deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("Unreachable-Regular.ttf"), b"").unwrap();

        let families: Vec<String> = font_rows_from_dirs(&[root.to_path_buf()])
            .into_iter()
            .map(|r| r.family)
            .collect();
        assert!(families.contains(&"Menlo".to_string()), "{families:?}");
        assert!(
            families.contains(&"Courier New".to_string()),
            "{families:?}"
        );
        assert!(
            families.contains(&"JetBrainsMono".to_string()),
            "{families:?}"
        );
        assert!(
            families.contains(&"FiraCodeNerdFont".to_string()),
            "the nix-store nesting is the case that made this a bug: {families:?}"
        );
        assert!(
            !families.contains(&"Unreachable".to_string()),
            "depth cap must hold: {families:?}"
        );
    }

    #[test]
    fn dir_enumeration_dedupes_faces_and_ranks_like_fc_list() {
        let dir = std::env::temp_dir().join(format!("tg-fontdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in [
            "JetBrainsMonoNerdFont-Regular.ttf",
            "JetBrainsMonoNerdFont-Bold.ttf",
            "JetBrainsMonoNerdFont-Italic.otf",
            "Menlo.ttc",
            "NotAFont.txt", // wrong extension — ignored
            "README",       // no extension at all — ignored
        ] {
            std::fs::write(dir.join(f), b"").expect("write");
        }
        let rows = font_rows_from_dirs(std::slice::from_ref(&dir));
        let families: Vec<&str> = rows.iter().map(|r| r.family.as_str()).collect();
        assert_eq!(families.len(), 2, "rows: {families:?}");
        assert!(families.contains(&"Menlo"));
        assert!(families.contains(&"JetBrainsMonoNerdFont"));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        // A missing directory is skipped, not an error.
        assert!(font_rows_from_dirs(&[PathBuf::from("/no/such/dir")]).is_empty());
    }

    #[test]
    fn parses_fc_list_families_dedupes_and_prioritizes_recommended_fonts() {
        let fc_list = "\
FiraCode Nerd Font,FiraCode NF\n\
ZedMono Nerd Font,ZedMono NF\n\
JetBrainsMono Nerd Font,JetBrainsMono NF\n\
/path/FiraBold.ttf: FiraCode Nerd Font:style=Bold\n";

        let rows = font_rows_from_fc_list(fc_list);

        let labels: Vec<_> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels[0], "★ Recommended — JetBrainsMono Nerd Font");
        assert_eq!(labels[1], "★ Recommended — FiraCode Nerd Font");
        assert!(labels.contains(&"ZedMono Nerd Font"));
        assert_eq!(
            rows.iter()
                .filter(|row| row.family == "FiraCode Nerd Font")
                .count(),
            1
        );
    }

    #[test]
    fn patch_alacritty_font_family_updates_only_normal_family_line() {
        let input = "\
[font]\n\
normal = { family = \"FiraCode Nerd Font\" }\n\
size = 13\n\
# normal = { family = \"Commented\" }\n";

        let patched = patch_alacritty_font_family(input, "JetBrainsMono Nerd Font").unwrap();

        assert!(patched.contains("normal = { family = \"JetBrainsMono Nerd Font\" }"));
        assert!(patched.contains("# normal = { family = \"Commented\" }"));
        assert!(patched.contains("size = 13"));
    }
}
