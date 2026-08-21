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

/// Derive families from font FILENAMES in `dirs` — the fontconfig-free fallback.
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
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for ent in entries.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
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
    rank_families(families)
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

pub fn alacritty_config_path() -> PathBuf {
    std::env::var_os("THEGN_ALACRITTY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config/alacritty.toml"))
}

pub fn apply_font_family(family: &str) -> Result<PathBuf, String> {
    let path = alacritty_config_path();
    apply_font_family_to_path(&path, family)?;
    Ok(path)
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
    fn dir_enumeration_dedupes_faces_and_ranks_like_fc_list() {
        // `std::env::temp_dir()` + pid, matching the rest of this crate's tests
        // (thegn-host carries no tempfile dev-dependency).
        let dir = std::env::temp_dir().join(format!("tg-fontdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
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
