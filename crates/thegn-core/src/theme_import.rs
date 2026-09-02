//! Pure, bounded Gogh scheme parsing and palette conversion.
//!
//! The host owns path selection and file I/O.  This module receives bytes only
//! and accepts a deliberately narrow flat YAML grammar or a strict JSON object.

use std::collections::BTreeMap;

use crate::theme::{Hue, Palette, blend_over, contrast_ratio, extend_palette};
use crate::theme_resolve::normalize_hex;
use crate::theme_user::UserTheme;

/// Maximum size accepted by the pure importer.
pub const MAX_GOGH_BYTES: usize = 64 * 1024;

const FIELDS: &[&str] = &[
    "name",
    "variant",
    "background",
    "foreground",
    "cursor",
    "color_01",
    "color_02",
    "color_03",
    "color_04",
    "color_05",
    "color_06",
    "color_07",
    "color_08",
    "color_09",
    "color_10",
    "color_11",
    "color_12",
    "color_13",
    "color_14",
    "color_15",
    "color_16",
];

/// All sixteen Gogh ANSI values, retained as normalized hex values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ansi16 {
    pub colors: [String; 16],
}

impl Ansi16 {
    pub fn get(&self, index: usize) -> Option<&str> {
        self.colors.get(index).map(String::as_str)
    }
}

/// Parsed Gogh fields before conversion to thegn's palette roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoghScheme {
    pub name: String,
    pub variant: Option<String>,
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub ansi: Ansi16,
}

/// Structured errors for malformed or unsafe import input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeImportError {
    Oversized {
        size: usize,
        max: usize,
    },
    InvalidUtf8,
    InvalidJson(String),
    InvalidYaml {
        line: usize,
        message: String,
    },
    NotAnObject,
    UnknownField(String),
    DuplicateField(String),
    MissingField(String),
    NonStringField(String),
    InvalidVariant(String),
    VariantMismatch {
        variant: String,
        background: String,
        foreground: String,
    },
    UnsafeName,
    InvalidHex {
        field: String,
        value: String,
    },
}

impl std::fmt::Display for ThemeImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversized { size, max } => {
                write!(f, "theme import is {size} bytes; maximum is {max}")
            }
            Self::InvalidUtf8 => f.write_str("theme import is not UTF-8"),
            Self::InvalidJson(error) => write!(f, "invalid Gogh JSON: {error}"),
            Self::InvalidYaml { line, message } => {
                write!(f, "invalid Gogh YAML at line {line}: {message}")
            }
            Self::NotAnObject => f.write_str("Gogh import must be an object"),
            Self::UnknownField(field) => write!(f, "unsupported Gogh field: {field}"),
            Self::DuplicateField(field) => write!(f, "duplicate Gogh field: {field}"),
            Self::MissingField(field) => write!(f, "missing Gogh field: {field}"),
            Self::NonStringField(field) => write!(f, "Gogh field must be a scalar string: {field}"),
            Self::InvalidVariant(value) => write!(f, "unsupported Gogh variant: {value}"),
            Self::VariantMismatch {
                variant,
                background,
                foreground,
            } => write!(
                f,
                "Gogh {variant} variant conflicts with background {background} and foreground {foreground}"
            ),
            Self::UnsafeName => {
                f.write_str("Gogh theme name contains control or non-printing data")
            }
            Self::InvalidHex { field, value } => {
                write!(f, "invalid hex color for {field}: {value}")
            }
        }
    }
}

impl std::error::Error for ThemeImportError {}

/// Parse a bounded Gogh YAML or JSON document.
pub fn parse_gogh(input: &[u8]) -> Result<GoghScheme, ThemeImportError> {
    if input.len() > MAX_GOGH_BYTES {
        return Err(ThemeImportError::Oversized {
            size: input.len(),
            max: MAX_GOGH_BYTES,
        });
    }
    let text = std::str::from_utf8(input).map_err(|_| ThemeImportError::InvalidUtf8)?;
    if text.trim_start().starts_with('{') {
        parse_json(text)
    } else {
        parse_yaml(text)
    }
}

/// Alias used by callers that do not need to name the format.
pub fn parse(input: &[u8]) -> Result<GoghScheme, ThemeImportError> {
    parse_gogh(input)
}

/// Parse and convert a Gogh document to a validated user theme.
pub fn import_gogh(input: &[u8]) -> Result<UserTheme, ThemeImportError> {
    let scheme = parse_gogh(input)?;
    convert_gogh(&scheme)
}

/// Convert a parsed Gogh scheme using the fixed ANSI role table. Contradictory
/// variant metadata is rejected even for callers that construct a scheme
/// directly rather than going through [`parse_gogh`].
pub fn convert_gogh(scheme: &GoghScheme) -> Result<UserTheme, ThemeImportError> {
    validate_variant_contrast(
        scheme.variant.as_deref(),
        &scheme.background,
        &scheme.foreground,
    )?;
    let background = rgb(scheme.background.as_str());
    let foreground = rgb(scheme.foreground.as_str());
    let cursor = rgb(scheme.cursor.as_str());
    let ansi: [String; 16] = std::array::from_fn(|index| rgb(&scheme.ansi.colors[index]));
    let dark_anchor = blend_over(&ansi[0], &ansi[8], 0.35);
    let light_anchor = blend_over(&ansi[15], &ansi[7], 0.35);

    // Surfaces are relative blends, so a light Gogh scheme remains light and
    // a dark scheme remains dark without a hard-coded terminal assumption.
    let mut palette = Palette {
        bg0: background.clone(),
        bg1: blend_over(&dark_anchor, &background, 0.20),
        panel: blend_over(&dark_anchor, &background, 0.34),
        panel2: blend_over(&light_anchor, &background, 0.20),
        raise: blend_over(&light_anchor, &background, 0.34),
        border: blend_over(&dark_anchor, &light_anchor, 0.50),
        focus: cursor,
        text: foreground,
        dim: blend_over(&rgb(&scheme.foreground), &background, 0.24),
        faint: blend_over(&rgb(&scheme.foreground), &background, 0.48),
        ghost: blend_over(&rgb(&scheme.foreground), &background, 0.68),
        accent: String::new(),
        ghost2: String::new(),
        ghost3: String::new(),
        shadow_bg: String::new(),
        shadow_fg: String::new(),
        chip_fg: String::new(),
        activity_active: String::new(),
        activity_waiting: String::new(),
        activity_done: String::new(),
        hues: crate::theme::Hues::default(),
        heat: Default::default(),
    };

    let pairs = [
        (Hue::Red, 1usize, 9usize),
        (Hue::Green, 2, 10),
        (Hue::Amber, 3, 11),
        (Hue::Blue, 4, 12),
        (Hue::Purple, 5, 13),
        (Hue::Teal, 6, 14),
    ];
    let mut representatives = Vec::with_capacity(pairs.len());
    let mut losers = Vec::with_capacity(pairs.len());
    for (hue, normal, bright) in pairs {
        let normal_color = &ansi[normal];
        let bright_color = &ansi[bright];
        let (winner, loser) = if contrast_ratio(normal_color, &background)
            >= contrast_ratio(bright_color, &background)
        {
            (normal_color, bright_color)
        } else {
            (bright_color, normal_color)
        };
        representatives.push((hue, winner.clone()));
        losers.push(loser.clone());
    }
    for (hue, color) in &representatives {
        match hue {
            Hue::Red => palette.hues.red = color.clone(),
            Hue::Green => palette.hues.green = color.clone(),
            Hue::Amber => palette.hues.amber = color.clone(),
            Hue::Blue => palette.hues.blue = color.clone(),
            Hue::Purple => palette.hues.purple = color.clone(),
            Hue::Teal => palette.hues.teal = color.clone(),
            Hue::Magenta | Hue::Orange => unreachable!(),
        }
    }
    palette.hues.orange = blend_over(&palette.hues.amber, &palette.hues.red, 0.5);
    palette.hues.magenta = blend_over(&palette.hues.purple, &palette.hues.red, 0.5);
    // Fold the unselected bright/normal values into the frame. This keeps
    // every ANSI input meaningful while the six visible hue roles use the
    // higher-contrast member of each pair.
    let loser_mix = average(&losers);
    palette.border = blend_over(&loser_mix, &palette.border, 0.18);
    palette.accent = representatives
        .iter()
        .max_by(|(_, left), (_, right)| {
            contrast_ratio(left, &background).total_cmp(&contrast_ratio(right, &background))
        })
        .map(|(_, color)| color.clone())
        .unwrap_or_else(|| rgb(&scheme.foreground));
    extend_palette(&mut palette);

    let mut theme = UserTheme::from_palette(&scheme.name, &palette);
    theme.meta.variant = scheme.variant.clone();
    Ok(theme)
}

fn parse_json(text: &str) -> Result<GoghScheme, ThemeImportError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| ThemeImportError::InvalidJson(error.to_string()))?;
    let object = value.as_object().ok_or(ThemeImportError::NotAnObject)?;
    let mut fields = BTreeMap::new();
    for (key, value) in object {
        if !FIELDS.contains(&key.as_str()) {
            return Err(ThemeImportError::UnknownField(key.clone()));
        }
        if fields
            .insert(key.clone(), value.as_str().map(str::to_owned))
            .is_some()
        {
            return Err(ThemeImportError::DuplicateField(key.clone()));
        }
        if !value.is_string() {
            return Err(ThemeImportError::NonStringField(key.clone()));
        }
    }
    from_fields(fields)
}

fn parse_yaml(text: &str) -> Result<GoghScheme, ThemeImportError> {
    let mut fields = BTreeMap::new();
    for (line_index, raw_line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line == "---" || line == "..." || line.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once(':') else {
            return Err(yaml_error(line_number, "expected a key: value pair"));
        };
        let key = raw_key.trim();
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            return Err(yaml_error(line_number, "invalid field name"));
        }
        if !FIELDS.contains(&key) {
            return Err(ThemeImportError::UnknownField(key.into()));
        }
        let value = parse_yaml_scalar(raw_value.trim(), line_number)?;
        if fields.insert(key.into(), Some(value)).is_some() {
            return Err(ThemeImportError::DuplicateField(key.into()));
        }
    }
    from_fields(fields)
}

fn parse_yaml_scalar(value: &str, line: usize) -> Result<String, ThemeImportError> {
    if value.is_empty()
        || value.starts_with('&')
        || value.starts_with('*')
        || value.starts_with('!')
    {
        return Err(yaml_error(line, "value must be a scalar"));
    }
    if value.starts_with('[')
        || value.starts_with('{')
        || value.starts_with('|')
        || value.starts_with('>')
        || value.starts_with('-')
    {
        return Err(yaml_error(line, "collections and code are not accepted"));
    }
    let value = strip_yaml_comment(value);
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str(value).map_err(|_| yaml_error(line, "invalid quoted scalar"));
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    if value.contains('\n') || value.contains('\r') {
        return Err(yaml_error(line, "multiline scalar is not accepted"));
    }
    Ok(value.to_owned())
}

fn strip_yaml_comment(value: &str) -> &str {
    value
        .find(" #")
        .map(|index| &value[..index])
        .unwrap_or(value)
        .trim()
}

fn from_fields(fields: BTreeMap<String, Option<String>>) -> Result<GoghScheme, ThemeImportError> {
    let required = |name: &str| {
        fields
            .get(name)
            .and_then(Option::as_deref)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ThemeImportError::MissingField(name.into()))
    };
    let name = required("name")?.to_owned();
    if name.trim().is_empty() {
        return Err(ThemeImportError::MissingField("name".into()));
    }
    if !crate::theme_user::safe_theme_name(&name) {
        return Err(ThemeImportError::UnsafeName);
    }
    let variant = fields
        .get("variant")
        .and_then(|value| value.as_deref())
        .map(str::to_ascii_lowercase);
    if let Some(variant) = &variant
        && variant != "dark"
        && variant != "light"
    {
        return Err(ThemeImportError::InvalidVariant(variant.clone()));
    }
    let color = |name: &str| -> Result<String, ThemeImportError> {
        let value = required(name)?;
        normalize_hex(value).ok_or_else(|| ThemeImportError::InvalidHex {
            field: name.into(),
            value: value.into(),
        })
    };
    let mut colors = std::array::from_fn(|_| String::new());
    for (index, slot) in colors.iter_mut().enumerate() {
        *slot = color(&format!("color_{:02}", index + 1))?;
    }
    let background = color("background")?;
    let foreground = color("foreground")?;
    validate_variant_contrast(variant.as_deref(), &background, &foreground)?;
    Ok(GoghScheme {
        name,
        variant,
        background,
        foreground,
        cursor: color("cursor")?,
        ansi: Ansi16 { colors },
    })
}

fn validate_variant_contrast(
    variant: Option<&str>,
    background: &str,
    foreground: &str,
) -> Result<(), ThemeImportError> {
    let Some(variant) = variant else {
        return Ok(());
    };
    let background_luma = crate::theme::relative_luminance(&rgb(background));
    let foreground_luma = crate::theme::relative_luminance(&rgb(foreground));
    let consistent = match variant {
        "light" => background_luma > foreground_luma,
        "dark" => background_luma < foreground_luma,
        _ => return Err(ThemeImportError::InvalidVariant(variant.into())),
    };
    if consistent {
        Ok(())
    } else {
        Err(ThemeImportError::VariantMismatch {
            variant: variant.into(),
            background: background.into(),
            foreground: foreground.into(),
        })
    }
}

fn yaml_error(line: usize, message: impl Into<String>) -> ThemeImportError {
    ThemeImportError::InvalidYaml {
        line,
        message: message.into(),
    }
}

fn rgb(hex: &str) -> String {
    crate::theme_resolve::parse_hex_rgb(hex).unwrap_or_default()
}

fn average(colors: &[String]) -> String {
    let mut total = [0u32; 3];
    for color in colors {
        let mut channels = color
            .split(';')
            .map(|channel| channel.parse::<u32>().unwrap_or(0));
        for channel in &mut total {
            *channel += channels.next().unwrap_or(0);
        }
    }
    let divisor = colors.len().max(1) as u32;
    format!(
        "{};{};{}",
        total[0] / divisor,
        total[1] / divisor,
        total[2] / divisor
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme_contrast::{Bar, audit};

    fn yaml() -> String {
        let mut text = String::from(
            "name: \"Test Gogh\"\nvariant: dark\nbackground: \"#101820\"\nforeground: \"#f0f0f0\"\ncursor: \"#00ffcc\"\n",
        );
        for index in 1..=16 {
            text.push_str(&format!(
                "color_{index:02}: \"#{:02x}{:02x}{:02x}\"\n",
                index,
                index + 1,
                index + 2
            ));
        }
        text
    }

    #[test]
    fn yaml_parser_retains_all_sixteen_values_and_mappings() {
        let scheme = parse_gogh(yaml().as_bytes()).unwrap();
        assert_eq!(scheme.ansi.colors.len(), 16);
        assert_eq!(scheme.ansi.get(0), Some("#010203"));
        assert_eq!(scheme.ansi.get(15), Some("#101112"));
        assert_eq!(scheme.background, "#101820");
        assert_eq!(scheme.foreground, "#f0f0f0");
        assert_eq!(scheme.cursor, "#00ffcc");
        let theme = convert_gogh(&scheme).unwrap();
        let palette = crate::theme_resolve::palette_from_user_theme(&theme).unwrap();
        assert_eq!(palette.bg0, "16;24;32");
        assert_eq!(palette.text, "240;240;240");
        assert_eq!(palette.focus, "0;255;204");
        assert!(!palette.ghost2.is_empty());
        assert!(
            audit(&palette, Bar::Preset)
                .iter()
                .all(|finding| finding.ratio.is_finite())
        );
    }

    #[test]
    fn json_and_light_variant_are_supported() {
        let mut object = serde_json::json!({
            "name": "Light",
            "variant": "light",
            "background": "#f8f7f2",
            "foreground": "#202020",
            "cursor": "#006666",
        });
        for index in 1..=16 {
            object[format!("color_{index:02}")] =
                serde_json::json!(format!("#{:02x}{:02x}{:02x}", index, index, index));
        }
        let bytes = serde_json::to_vec(&object).unwrap();
        let theme = import_gogh(&bytes).unwrap();
        assert_eq!(theme.meta.variant.as_deref(), Some("light"));
        let palette = crate::theme_resolve::palette_from_user_theme(&theme).unwrap();
        assert!(luma(&palette.bg0) > luma(&palette.text));
    }

    #[test]
    fn contradictory_light_and_dark_variants_are_rejected() {
        let mut direct = parse_gogh(yaml().as_bytes()).unwrap();
        direct.variant = Some("light".into());
        assert!(matches!(
            convert_gogh(&direct),
            Err(ThemeImportError::VariantMismatch { variant, .. }) if variant == "light"
        ));

        let light_with_dark_surface = yaml()
            .replacen("variant: dark", "variant: light", 1)
            .replacen("background: \"#101820\"", "background: \"#101010\"", 1);
        assert!(matches!(
            parse_gogh(light_with_dark_surface.as_bytes()),
            Err(ThemeImportError::VariantMismatch { variant, .. }) if variant == "light"
        ));

        let dark_with_light_surface = yaml()
            .replacen("background: \"#101820\"", "background: \"#f8f8f8\"", 1)
            .replacen("foreground: \"#f0f0f0\"", "foreground: \"#202020\"", 1);
        assert!(matches!(
            parse_gogh(dark_with_light_surface.as_bytes()),
            Err(ThemeImportError::VariantMismatch { variant, .. }) if variant == "dark"
        ));
    }

    #[test]
    fn malformed_missing_and_oversized_input_is_rejected() {
        assert!(matches!(
            parse_gogh(b"name: x\n"),
            Err(ThemeImportError::MissingField(_))
        ));
        assert!(matches!(
            parse_gogh(b"name: x\nbackground: '#12zz00'\n"),
            Err(ThemeImportError::MissingField(_))
        ));
        assert!(matches!(
            parse_gogh(&vec![b'x'; MAX_GOGH_BYTES + 1]),
            Err(ThemeImportError::Oversized { .. })
        ));
        assert!(matches!(
            parse_gogh(b"{\"name\": 1}"),
            Err(ThemeImportError::NonStringField(_))
        ));
        assert!(matches!(
            parse_gogh(b"name: x\nnope: y\n"),
            Err(ThemeImportError::UnknownField(_))
        ));
    }

    #[test]
    fn hostile_metadata_name_is_rejected_before_import() {
        let input = yaml().replacen("name: \"Test Gogh\"", "name: bad\u{1b}name", 1);
        assert!(matches!(
            parse_gogh(input.as_bytes()),
            Err(ThemeImportError::UnsafeName)
        ));
        let mut theme = convert_gogh(&parse_gogh(yaml().as_bytes()).unwrap()).unwrap();
        theme.meta.name = "bad\u{1b}name".into();
        assert!(matches!(
            theme.validate(),
            Err(crate::theme_user::UserThemeError::UnsafeName)
        ));
    }

    #[test]
    fn whitespace_only_name_is_rejected_before_conversion() {
        let input = yaml().replacen("name: \"Test Gogh\"", "name: '   '", 1);
        assert!(matches!(
            import_gogh(input.as_bytes()),
            Err(ThemeImportError::MissingField(field)) if field == "name"
        ));
    }

    fn luma(rgb: &str) -> u32 {
        rgb.split(';').map(|v| v.parse::<u32>().unwrap()).sum()
    }
}
