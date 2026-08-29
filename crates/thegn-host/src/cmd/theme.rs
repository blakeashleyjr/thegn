//! `thegn theme` — list, select, and import themes without launching the TUI.

use anyhow::Result;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::theme::{self, PRESETS};
use thegn_core::theme_user::UserTheme;
use thegn_core::{msg, outln, util};

const MAX_THEME_FILES: usize = 256;

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
    if PRESETS.contains(&name) {
        write_preset(config_path, name)?;
    } else if let Some(user) = read_user_themes()
        .into_iter()
        .find(|theme| theme.meta.name == name)
    {
        // Config resolution knows the existing preset key and override tables;
        // materialize a user theme's roles there so selecting its name remains
        // a normal `[theme].preset`, not a second config key.
        write_theme_config(config_path, name, &user)?;
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
    let bytes = read_bounded(path)?;
    let mut theme = thegn_core::theme_import::import_gogh(&bytes)?;
    if let Some(name) = name {
        theme.meta.name = name.to_owned();
    }
    theme.validate()?;
    let dir = util::xdg_config_home().join("thegn/themes");
    write_user_theme(&dir, &theme)?;
    msg::info(&format!("theme imported as `{}`", theme.meta.name));
    Ok(())
}

fn read_user_themes() -> Vec<UserTheme> {
    let dir = util::xdg_config_home().join("thegn/themes");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut themes = Vec::new();
    for entry in entries.flatten().take(MAX_THEME_FILES) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let Ok(bytes) = read_bounded(&path) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if let Ok(theme) = UserTheme::from_toml(text) {
            themes.push(theme);
        }
    }
    themes.sort_by(|left, right| left.meta.name.cmp(&right.meta.name));
    themes
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("theme source is not a regular file: {}", path.display());
    }
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if size > crate::theme_store::MAX_THEME_FILE_BYTES {
        anyhow::bail!(
            "theme file is {size} bytes; maximum is {}",
            crate::theme_store::MAX_THEME_FILE_BYTES
        );
    }
    Ok(std::fs::read(path)?)
}

fn valid_slug(name: &str) -> Result<String> {
    let slug = util::slugify(name);
    if slug.is_empty() || slug.len() > 80 {
        anyhow::bail!("theme name must produce a non-empty slug of at most 80 characters");
    }
    Ok(slug)
}

fn write_user_theme(dir: &Path, theme: &UserTheme) -> Result<()> {
    theme.validate()?;
    std::fs::create_dir_all(dir)?;
    let slug = valid_slug(&theme.meta.name)?;
    let destination = dir.join(format!("{slug}.toml"));
    let temporary = destination.with_extension("toml.tmp");
    std::fs::write(&temporary, theme.to_toml()?)?;
    std::fs::rename(temporary, destination)?;
    Ok(())
}

fn write_preset(path: &Path, preset: &str) -> Result<()> {
    let mut doc = read_document(path)?;
    let theme = doc
        .entry("theme")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[theme] is not a table"))?;
    theme.insert("preset", toml_edit::value(preset));
    write_document(path, &doc)
}

fn write_theme_config(path: &Path, preset: &str, user: &UserTheme) -> Result<()> {
    let mut doc = read_document(path)?;
    let root = doc.as_table_mut();
    let theme = root
        .entry("theme")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[theme] is not a table"))?;
    theme.insert("preset", toml_edit::value(preset));
    theme.insert("accent", toml_edit::value(&user.colors.accent));
    theme.insert("focus_border", toml_edit::value(&user.colors.focus));
    let colors = theme
        .entry("colors")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[theme.colors] is not a table"))?;
    for (key, value) in [
        ("bg0", &user.colors.bg0),
        ("bg1", &user.colors.bg1),
        ("panel", &user.colors.panel),
        ("panel2", &user.colors.panel2),
        ("raise", &user.colors.raise),
        ("border", &user.colors.border),
        ("text", &user.colors.text),
        ("dim", &user.colors.dim),
        ("faint", &user.colors.faint),
        ("ghost", &user.colors.ghost),
    ] {
        colors.insert(key, toml_edit::value(value));
    }
    let hues = theme
        .entry("hues")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[theme.hues] is not a table"))?;
    for (key, value) in [
        ("teal", &user.hues.teal),
        ("magenta", &user.hues.magenta),
        ("purple", &user.hues.purple),
        ("green", &user.hues.green),
        ("amber", &user.hues.amber),
        ("red", &user.hues.red),
        ("blue", &user.hues.blue),
        ("orange", &user.hues.orange),
    ] {
        hues.insert(key, toml_edit::value(value));
    }
    write_document(path, &doc)
}

fn read_document(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|error| anyhow::anyhow!("parse {}: {error}", path.display()))
}

fn write_document(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, doc.to_string())?;
    std::fs::rename(temporary, path)?;
    Ok(())
}
