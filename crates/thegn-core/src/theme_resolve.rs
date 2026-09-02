//! Pure palette resolution shared by config, user themes, and the builder.

use crate::config::{ThemeColors, ThemeHues};
use crate::theme::{Palette, extend_palette, preset};
use crate::theme_user::UserTheme;

pub(crate) const DEFAULTISH_ACCENTS: &[&str] = &["#6ee7d8", "#76eede"];
pub(crate) const DEFAULTISH_FOCUS: &[&str] = &["#6ee7d8", "#9bd1ff"];

/// Resolve a built-in preset and apply the existing config overrides.
pub fn palette_with_config(
    preset_name: &str,
    colors: &ThemeColors,
    hues: &ThemeHues,
    accent: &str,
    focus: &str,
) -> Palette {
    palette_with_catalog(preset_name, &[], colors, hues, accent, focus)
}

/// Resolve a named palette from the built-in and loaded user-theme catalogs.
/// Built-ins deliberately win name collisions, and config-shaped overrides are
/// applied after either base palette so user themes behave exactly like presets.
pub fn palette_with_catalog(
    preset_name: &str,
    user_themes: &[UserTheme],
    colors: &ThemeColors,
    hues: &ThemeHues,
    accent: &str,
    focus: &str,
) -> Palette {
    let mut palette = preset(preset_name)
        .or_else(|| {
            user_themes
                .iter()
                .find(|theme| theme.meta.name == preset_name)
                .and_then(|theme| palette_from_user_theme(theme).ok())
        })
        .unwrap_or_default();
    apply_config_overrides(&mut palette, colors, hues, accent, focus);
    extend_palette(&mut palette);
    palette
}

/// Apply config-shaped overrides to a palette without performing derivation.
/// Callers can use this to layer a config over a user theme before extending.
pub fn apply_config_overrides(
    palette: &mut Palette,
    colors: &ThemeColors,
    hues: &ThemeHues,
    accent: &str,
    focus: &str,
) {
    set(&mut palette.bg0, &colors.bg0);
    set(&mut palette.bg1, &colors.bg1);
    set(&mut palette.panel, &colors.panel);
    set(&mut palette.panel_alt, &colors.panel_alt);
    set(&mut palette.panel2, &colors.panel2);
    set(&mut palette.raise, &colors.raise);
    set(&mut palette.border, &colors.border);
    set(&mut palette.text, &colors.text);
    set(&mut palette.dim, &colors.dim);
    set(&mut palette.faint, &colors.faint);
    set(&mut palette.ghost, &colors.ghost);
    set(&mut palette.ghost2, &colors.ghost2);
    set(&mut palette.ghost3, &colors.ghost3);
    set(&mut palette.shadow_bg, &colors.shadow_bg);
    set(&mut palette.shadow_fg, &colors.shadow_fg);
    set(&mut palette.chip_fg, &colors.chip_fg);
    set(&mut palette.activity_active, &colors.activity_active);
    set(&mut palette.activity_waiting, &colors.activity_waiting);
    set(&mut palette.activity_done, &colors.activity_done);

    set(&mut palette.hues.teal, &hues.teal);
    set(&mut palette.hues.magenta, &hues.magenta);
    set(&mut palette.hues.purple, &hues.purple);
    set(&mut palette.hues.green, &hues.green);
    set(&mut palette.hues.amber, &hues.amber);
    set(&mut palette.hues.red, &hues.red);
    set(&mut palette.hues.blue, &hues.blue);
    set(&mut palette.hues.orange, &hues.orange);

    // The defaults intentionally do not clobber the accent/focus selected by
    // a preset. This preserves the long-standing cycle behavior.
    if !DEFAULTISH_FOCUS.contains(&focus)
        && let Some(rgb) = parse_hex_rgb(focus)
    {
        palette.focus = rgb;
    }
    if !DEFAULTISH_ACCENTS.contains(&accent)
        && let Some(rgb) = parse_hex_rgb(accent)
    {
        palette.accent = rgb;
    }
}

/// Resolve a validated user theme, deriving all extension tokens.
pub fn palette_from_user_theme(
    theme: &UserTheme,
) -> Result<Palette, crate::theme_user::UserThemeError> {
    theme.validate()?;
    let c = &theme.colors;
    let mut palette = Palette {
        bg0: required_rgb(&c.bg0),
        bg1: required_rgb(&c.bg1),
        panel: required_rgb(&c.panel),
        panel_alt: String::new(),
        panel2: required_rgb(&c.panel2),
        raise: required_rgb(&c.raise),
        border: required_rgb(&c.border),
        focus: required_rgb(&c.focus),
        text: required_rgb(&c.text),
        dim: required_rgb(&c.dim),
        faint: required_rgb(&c.faint),
        ghost: required_rgb(&c.ghost),
        accent: required_rgb(&c.accent),
        ghost2: String::new(),
        ghost3: String::new(),
        shadow_bg: String::new(),
        shadow_fg: String::new(),
        chip_fg: String::new(),
        activity_active: String::new(),
        activity_waiting: String::new(),
        activity_done: String::new(),
        hues: crate::theme::Hues {
            teal: required_rgb(&theme.hues.teal),
            magenta: required_rgb(&theme.hues.magenta),
            purple: required_rgb(&theme.hues.purple),
            green: required_rgb(&theme.hues.green),
            amber: required_rgb(&theme.hues.amber),
            red: required_rgb(&theme.hues.red),
            blue: required_rgb(&theme.hues.blue),
            orange: required_rgb(&theme.hues.orange),
        },
        heat: Default::default(),
    };
    extend_palette(&mut palette);
    Ok(palette)
}

/// Resolve a user theme and then layer the regular config overrides over it.
pub fn palette_with_user_theme(
    theme: &UserTheme,
    colors: &ThemeColors,
    hues: &ThemeHues,
    accent: &str,
    focus: &str,
) -> Result<Palette, crate::theme_user::UserThemeError> {
    let mut palette = palette_from_user_theme(theme)?;
    apply_config_overrides(&mut palette, colors, hues, accent, focus);
    extend_palette(&mut palette);
    Ok(palette)
}

fn set(slot: &mut String, value: &Option<String>) {
    if let Some(rgb) = value.as_deref().and_then(parse_hex_rgb) {
        *slot = rgb;
    }
}

fn required_rgb(value: &str) -> String {
    parse_hex_rgb(value).unwrap_or_default()
}

/// Parse `#rgb` or `#rrggbb` into the palette's truecolor fragment.
pub(crate) fn parse_hex_rgb(hex: &str) -> Option<String> {
    let h = hex.trim().strip_prefix('#')?;
    let h = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => h.to_string(),
        _ => return None,
    };
    let n = u32::from_str_radix(&h, 16).ok()?;
    Some(format!(
        "{};{};{}",
        (n >> 16) & 255,
        (n >> 8) & 255,
        n & 255
    ))
}

/// Normalize a hex color to lowercase six-digit form.
pub(crate) fn normalize_hex(hex: &str) -> Option<String> {
    let rgb = parse_hex_rgb(hex)?;
    let mut channels = rgb.split(';').map(str::parse::<u8>);
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channels.next()?.ok()?,
        channels.next()?.ok()?,
        channels.next()?.ok()?
    ))
}

/// Convert a palette fragment to a stable six-digit hex color.
pub(crate) fn rgb_to_hex(rgb: &str) -> String {
    let mut channels = rgb.split(';').map(str::parse::<u8>);
    match (channels.next(), channels.next(), channels.next()) {
        (Some(Ok(r)), Some(Ok(g)), Some(Ok(b))) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#000000".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ThemeColors, ThemeHues};
    use crate::theme::PRESETS;

    #[test]
    fn config_resolution_preserves_preset_defaultish_semantics() {
        let defaults = ThemeColors::default();
        let hues = ThemeHues::default();
        let prism = palette_with_config("prism", &defaults, &hues, "#6ee7d8", "#6ee7d8");
        let light = palette_with_config("light", &defaults, &hues, "#6ee7d8", "#6ee7d8");
        assert_ne!(prism.bg0, light.bg0);
        assert_eq!(prism.accent, crate::theme::HUE_TEAL);
        assert_ne!(light.focus, prism.focus);
    }

    #[test]
    fn every_builtin_still_resolves_and_extensions_are_filled() {
        for name in PRESETS {
            let palette = palette_with_config(
                name,
                &ThemeColors::default(),
                &ThemeHues::default(),
                "#6ee7d8",
                "#6ee7d8",
            );
            assert!(!palette.ghost2.is_empty(), "{name}");
            assert!(!palette.heat[4].is_empty(), "{name}");
        }
    }

    #[test]
    fn user_theme_resolution_survives_reload_and_layers_config_overrides() {
        let mut base = Palette::default();
        extend_palette(&mut base);
        let user = UserTheme::from_palette("local-paper", &base);
        let colors = ThemeColors {
            bg0: Some("#123456".into()),
            ..ThemeColors::default()
        };
        let hues = ThemeHues::default();

        let first = palette_with_catalog(
            "local-paper",
            std::slice::from_ref(&user),
            &colors,
            &hues,
            "#6ee7d8",
            "#6ee7d8",
        );
        let after_reload = palette_with_catalog(
            "local-paper",
            std::slice::from_ref(&user),
            &colors,
            &hues,
            "#6ee7d8",
            "#6ee7d8",
        );
        assert_eq!(first, after_reload);
        assert_eq!(first.bg0, "18;52;86");
        assert_eq!(
            palette_with_catalog(
                "prism",
                std::slice::from_ref(&user),
                &colors,
                &hues,
                "#6ee7d8",
                "#6ee7d8",
            )
            .bg0,
            "18;52;86"
        );
    }

    #[test]
    fn hex_helpers_normalize_short_and_reject_bad_values() {
        assert_eq!(parse_hex_rgb("#abc"), Some("170;187;204".into()));
        assert_eq!(normalize_hex("#ABC"), Some("#aabbcc".into()));
        assert!(parse_hex_rgb("#12zz00").is_none());
        assert!(parse_hex_rgb("12ff00").is_none());
    }
}
