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
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(font_rows_from_fc_list(&stdout)
        .into_iter()
        .map(|row| PaletteItem::new(format!("font:{}", row.family), row.label))
        .collect())
}

pub fn font_rows_from_fc_list(fc_list: &str) -> Vec<FontRow> {
    let recommended_order: BTreeMap<String, usize> = RECOMMENDED_FONTS
        .iter()
        .enumerate()
        .map(|(idx, name)| (normalize_family(name), idx))
        .collect();

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
