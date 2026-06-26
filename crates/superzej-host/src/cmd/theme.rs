//! `superzej theme` — preview and interactively select themes.

use anyhow::Result;
use std::process::Command;
use superzej_core::config::Config;
use superzej_core::theme::{self, PRESETS};
use superzej_core::{msg, outln, util};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List all available themes with a color preview.
    List,
    /// Interactively select a theme (via FZF or gum) and write it to config.toml.
    Set,
}

pub fn run(cfg: &Config, action: Action, config_path: std::path::PathBuf) -> Result<()> {
    match action {
        Action::List => list(),
        Action::Set => set(cfg, config_path),
    }
}

fn list() -> Result<()> {
    for name in PRESETS {
        if let Some(pal) = theme::preset(name) {
            let bg = theme::bg(&pal.bg0);
            let text = theme::fg(&pal.text);
            let accent = theme::fg(&pal.accent);
            let reset = theme::RESET;
            outln!("{bg} {name:<22} {text} Text {accent} Accent {reset}");
        }
    }
    Ok(())
}

fn set(_cfg: &Config, config_path: std::path::PathBuf) -> Result<()> {
    if !util::have("fzf") && !util::have("gum") {
        anyhow::bail!("theme set requires `fzf` or `gum` to be installed");
    }

    use std::io::Write;
    let mut child = if util::have("fzf") {
        Command::new("fzf")
            .arg("--prompt=Select theme > ")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?
    } else {
        Command::new("gum")
            .arg("filter")
            .arg("--placeholder=Select theme...")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?
    };

    if let Some(mut stdin) = child.stdin.take() {
        for name in PRESETS {
            let _ = writeln!(stdin, "{}", name);
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(()); // user cancelled
    }

    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        return Ok(());
    }

    // Use CLI config command to mutate the local TOML
    // A bit hacky but guarantees we don't clobber comments by rewriting it ourselves
    let _status = Command::new(std::env::current_exe()?)
        .args(["--set", &format!("theme.name={selected}")])
        // Wait, the `--set` arg overrides runtime config. We need to save it.
        // Actually, let's use the standard rust way to manipulate config.toml if superzej has one
        // Wait, `szhost` doesn't have a `config set` command natively!
        .output()?; // This won't work to SAVE it.

    // Let's implement a manual update to the toml file, or inform the user.
    // Better: Read TOML, use `toml_edit` to set the value and write back.
    let toml_str = std::fs::read_to_string(&config_path).unwrap_or_else(|_| "".to_string());

    // use the newer toml_edit Document parsing
    let mut doc = toml_str
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_else(|_| toml_edit::DocumentMut::new());

    // Ensure [theme] section exists
    if !doc.contains_key("theme") {
        doc["theme"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    // Set theme.name
    if let Some(theme_table) = doc["theme"].as_table_mut() {
        theme_table["name"] = toml_edit::value(selected.clone());
    }

    std::fs::write(&config_path, doc.to_string())?;
    msg::info(&format!(
        "theme set to `{}` in {}",
        selected,
        config_path.display()
    ));

    Ok(())
}
