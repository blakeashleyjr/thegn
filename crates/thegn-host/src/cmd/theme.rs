//! `thegn theme` — list, select, and import themes without launching the TUI.

use anyhow::Result;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::theme::{self, PRESETS};
use thegn_core::theme_contrast::{self, Bar};
use thegn_core::theme_user::UserTheme;
use thegn_core::{msg, outln, util};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List all available built-in and valid local themes.
    List,
    /// Select a theme by its built-in or local name.
    Set {
        /// Built-in preset or local theme name.
        name: String,
    },
    /// Import a local Gogh YAML or JSON file as a user theme.
    Import {
        /// Local Gogh scheme path.
        file: PathBuf,
        /// Override the name in the Gogh document.
        #[arg(long)]
        name: Option<String>,
    },
}

pub fn run(_cfg: &Config, action: Action, config_path: PathBuf) -> Result<()> {
    match action {
        Action::List => list(),
        Action::Set { name } => set(&name, &config_path),
        Action::Import { file, name } => import(&file, name.as_deref()),
    }
}

fn list() -> Result<()> {
    let users = read_user_themes();
    for name in PRESETS {
        if let Some(pal) = theme::preset(name) {
            print_preview(name, &pal);
        }
    }
    for user in users {
        // Built-ins are authoritative when a local file has the same name.
        if PRESETS.contains(&user.meta.name.as_str()) {
            continue;
        }
        if let Ok(pal) = user.palette() {
            print_preview(&user.meta.name, &pal);
        }
    }
    Ok(())
}

fn print_preview(name: &str, pal: &theme::Palette) {
    let bg = theme::bg(&pal.bg0);
    let text = theme::fg(&pal.text);
    let accent = theme::fg(&pal.accent);
    let reset = theme::RESET;
    outln!("{bg} {name:<22} {text} Text {accent} Accent {reset}");
}

fn set(name: &str, config_path: &Path) -> Result<()> {
    let known = PRESETS.contains(&name)
        || read_user_themes()
            .iter()
            .any(|theme| theme.meta.name == name);
    if known {
        write_selection(config_path, name)?;
    } else {
        anyhow::bail!("unknown theme `{name}`; run `thegn theme list`");
    }
    msg::info(&format!(
        "theme set to `{name}` in {}",
        config_path.display()
    ));
    Ok(())
}

fn import(path: &Path, name: Option<&str>) -> Result<()> {
    let bytes = crate::theme_store::read_bounded(path).map_err(anyhow::Error::msg)?;
    let mut theme = thegn_core::theme_import::import_gogh(&bytes)?;
    if let Some(name) = name {
        theme.meta.name = name.to_owned();
    }
    theme.validate()?;
    let dir = util::xdg_config_home().join("thegn/themes");
    crate::theme_store::write_theme(&dir, &theme).map_err(anyhow::Error::msg)?;
    report_contrast_warnings(&theme);
    msg::info(&format!("theme imported as `{}`", theme.meta.name));
    Ok(())
}

fn report_contrast_warnings(theme: &UserTheme) {
    let Ok(palette) = theme.palette() else {
        return;
    };
    for finding in theme_contrast::audit(&palette, Bar::Preset) {
        msg::warn(&format!(
            "contrast warning: {} on {} {:.2} < {:.1}",
            finding.fg, finding.bg, finding.ratio, finding.min
        ));
    }
}

fn read_user_themes() -> Vec<UserTheme> {
    let dir = util::xdg_config_home().join("thegn/themes");
    let (themes, warnings) = crate::theme_store::scan_dir(&dir);
    for warning in warnings {
        msg::warn(&warning);
    }
    themes
}

fn write_selection(path: &Path, preset: &str) -> Result<()> {
    crate::theme_store::write_theme_selection(path, preset, None).map_err(anyhow::Error::msg)
}
