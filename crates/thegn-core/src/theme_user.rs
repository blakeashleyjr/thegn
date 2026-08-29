//! The closed, versioned on-disk model for a user theme.
//!
//! User themes deliberately contain only editable base roles.  Structural
//! extension tokens are derived by [`crate::theme::extend_palette`] when the
//! theme is resolved, so a saved theme cannot become stale as the palette
//! contract grows.

use serde::{Deserialize, Serialize};

/// The only user-theme file format currently understood by thegn.
pub const USER_THEME_VERSION: u16 = 1;

/// Metadata stored in the `[meta]` table of a user theme.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserThemeMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// Editable surface, text, frame, accent, and focus roles.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserThemeColors {
    pub bg0: String,
    pub bg1: String,
    pub panel: String,
    pub panel2: String,
    pub raise: String,
    pub border: String,
    pub text: String,
    pub dim: String,
    pub faint: String,
    pub ghost: String,
    pub accent: String,
    pub focus: String,
}

/// The eight semantic hue roles used by chrome.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserThemeHues {
    pub teal: String,
    pub magenta: String,
    pub purple: String,
    pub green: String,
    pub amber: String,
    pub red: String,
    pub blue: String,
    pub orange: String,
}

/// A validated user theme file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserTheme {
    pub version: u16,
    pub meta: UserThemeMeta,
    pub colors: UserThemeColors,
    pub hues: UserThemeHues,
}

/// Errors returned while decoding or validating a user-theme TOML file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserThemeError {
    Toml(String),
    UnsupportedVersion(u16),
    InvalidHex { field: String, value: String },
    EmptyName,
}

impl std::fmt::Display for UserThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Toml(error) => write!(f, "invalid user theme TOML: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported user theme version {version}")
            }
            Self::InvalidHex { field, value } => {
                write!(f, "invalid hex color for {field}: {value}")
            }
            Self::EmptyName => f.write_str("user theme name must not be empty"),
        }
    }
}

impl std::error::Error for UserThemeError {}

impl UserTheme {
    /// Construct a complete user theme from a resolved palette.
    pub fn from_palette(name: impl Into<String>, palette: &crate::theme::Palette) -> Self {
        Self {
            version: USER_THEME_VERSION,
            meta: UserThemeMeta {
                name: name.into(),
                variant: None,
                origin: None,
            },
            colors: UserThemeColors {
                bg0: crate::theme_resolve::rgb_to_hex(&palette.bg0),
                bg1: crate::theme_resolve::rgb_to_hex(&palette.bg1),
                panel: crate::theme_resolve::rgb_to_hex(&palette.panel),
                panel2: crate::theme_resolve::rgb_to_hex(&palette.panel2),
                raise: crate::theme_resolve::rgb_to_hex(&palette.raise),
                border: crate::theme_resolve::rgb_to_hex(&palette.border),
                text: crate::theme_resolve::rgb_to_hex(&palette.text),
                dim: crate::theme_resolve::rgb_to_hex(&palette.dim),
                faint: crate::theme_resolve::rgb_to_hex(&palette.faint),
                ghost: crate::theme_resolve::rgb_to_hex(&palette.ghost),
                accent: crate::theme_resolve::rgb_to_hex(&palette.accent),
                focus: crate::theme_resolve::rgb_to_hex(&palette.focus),
            },
            hues: UserThemeHues {
                teal: crate::theme_resolve::rgb_to_hex(&palette.hues.teal),
                magenta: crate::theme_resolve::rgb_to_hex(&palette.hues.magenta),
                purple: crate::theme_resolve::rgb_to_hex(&palette.hues.purple),
                green: crate::theme_resolve::rgb_to_hex(&palette.hues.green),
                amber: crate::theme_resolve::rgb_to_hex(&palette.hues.amber),
                red: crate::theme_resolve::rgb_to_hex(&palette.hues.red),
                blue: crate::theme_resolve::rgb_to_hex(&palette.hues.blue),
                orange: crate::theme_resolve::rgb_to_hex(&palette.hues.orange),
            },
        }
    }

    /// Decode and validate a user theme from TOML.
    pub fn from_toml(text: &str) -> Result<Self, UserThemeError> {
        let theme: Self =
            toml::from_str(text).map_err(|error| UserThemeError::Toml(error.to_string()))?;
        theme.validate()?;
        Ok(theme)
    }

    /// Serialize this theme in the stable user-theme format.
    pub fn to_toml(&self) -> Result<String, UserThemeError> {
        self.validate()?;
        toml::to_string_pretty(self).map_err(|error| UserThemeError::Toml(error.to_string()))
    }

    /// Validate the closed model before handing it to a host provider.
    pub fn validate(&self) -> Result<(), UserThemeError> {
        if self.version != USER_THEME_VERSION {
            return Err(UserThemeError::UnsupportedVersion(self.version));
        }
        if self.meta.name.trim().is_empty() {
            return Err(UserThemeError::EmptyName);
        }
        let fields = [
            ("colors.bg0", &self.colors.bg0),
            ("colors.bg1", &self.colors.bg1),
            ("colors.panel", &self.colors.panel),
            ("colors.panel2", &self.colors.panel2),
            ("colors.raise", &self.colors.raise),
            ("colors.border", &self.colors.border),
            ("colors.text", &self.colors.text),
            ("colors.dim", &self.colors.dim),
            ("colors.faint", &self.colors.faint),
            ("colors.ghost", &self.colors.ghost),
            ("colors.accent", &self.colors.accent),
            ("colors.focus", &self.colors.focus),
            ("hues.teal", &self.hues.teal),
            ("hues.magenta", &self.hues.magenta),
            ("hues.purple", &self.hues.purple),
            ("hues.green", &self.hues.green),
            ("hues.amber", &self.hues.amber),
            ("hues.red", &self.hues.red),
            ("hues.blue", &self.hues.blue),
            ("hues.orange", &self.hues.orange),
        ];
        for (field, value) in fields {
            if crate::theme_resolve::normalize_hex(value).is_none() {
                return Err(UserThemeError::InvalidHex {
                    field: field.into(),
                    value: value.clone(),
                });
            }
        }
        Ok(())
    }

    /// Resolve this theme through the shared palette derivation seam.
    pub fn palette(&self) -> Result<crate::theme::Palette, UserThemeError> {
        crate::theme_resolve::palette_from_user_theme(self)
    }
}

/// Resolve a valid user theme to a complete palette. Invalid programmatically
/// constructed values fall back to the normal palette; file input uses
/// [`UserTheme::from_toml`] and therefore reports those errors instead.
pub fn to_palette(theme: &UserTheme) -> crate::theme::Palette {
    theme.palette().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Palette, extend_palette};

    #[test]
    fn user_theme_round_trips_toml_and_keeps_closed_shape() {
        let mut palette = Palette::default();
        extend_palette(&mut palette);
        let theme = UserTheme::from_palette("paperback", &palette);
        let text = theme.to_toml().unwrap();
        let decoded = UserTheme::from_toml(&text).unwrap();
        assert_eq!(decoded, theme);
        assert!(UserTheme::from_toml(&text.replace("version = 1", "version = 2")).is_err());
        assert!(UserTheme::from_toml(&format!("{text}\nextra = true\n")).is_err());
    }

    #[test]
    fn user_theme_has_only_editable_roles_and_resolves_extensions() {
        let mut palette = Palette::default();
        palette.ghost2.clear();
        palette.ghost3.clear();
        palette.shadow_bg.clear();
        palette.shadow_fg.clear();
        let theme = UserTheme::from_palette("derived", &palette);
        let resolved = crate::theme_resolve::palette_from_user_theme(&theme).unwrap();
        assert!(!resolved.ghost2.is_empty());
        assert!(!resolved.ghost3.is_empty());
        assert!(!resolved.shadow_bg.is_empty());
        assert!(
            crate::theme_contrast::audit(&resolved, crate::theme_contrast::Bar::Preset)
                .iter()
                .all(|finding| finding.ratio.is_finite())
        );
    }
}
